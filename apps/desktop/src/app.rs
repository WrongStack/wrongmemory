mod graph_view;
mod ui;

pub use ui::{AppAssets, apply_platform_font};

use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::Value;
use wrongmemory_contracts::{Admission, ProjectInfo};
use wrongmemory_desktop::model::{
    EditDraft, Generation, Graph, Memory, Page, Query, preview, string,
};
use wrongmemory_desktop::preferences::{Appearance, Preferences};
use wrongmemory_desktop::service::{self, Change, Command, Failure, Read, Service};
use wrongmemory_ipc::DataRoot;

use ui::{
    Tone, banner, binding, caption, chip, duration, empty_state, field, kbd, now_ms, relative_time,
    rows, scalar, section,
};

gpui_kit::actions!(
    desktop,
    [
        Refresh,
        FocusSearch,
        TogglePalette,
        NextRecord,
        PreviousRecord,
        Escape,
        Quit,
        FocusNextItem,
        FocusPreviousItem
    ]
);

pub fn bind_keys(cx: &mut App) {
    // Global quit: Cmd+Q is the macOS convention; elsewhere the window close
    // button and Alt+F4 / the compositor close it. Quitting the window never
    // stops the shared daemon.
    cx.on_action(|_: &Quit, cx| cx.quit());
    if ui::MAC {
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.set_menus(
            [Menu::new("WrongMemory").items([MenuItem::action("Quit WrongMemory", Quit)])],
        );
    }
    cx.bind_keys([
        KeyBinding::new(binding("r"), Refresh, Some("WrongMemoryDesktop")),
        KeyBinding::new(binding("f"), FocusSearch, Some("WrongMemoryDesktop")),
        KeyBinding::new(binding("k"), TogglePalette, Some("WrongMemoryDesktop")),
        KeyBinding::new(binding("p"), TogglePalette, Some("WrongMemoryDesktop")),
        KeyBinding::new("down", NextRecord, Some("MemoryLibrary")),
        KeyBinding::new("up", PreviousRecord, Some("MemoryLibrary")),
        KeyBinding::new("escape", Escape, Some("WrongMemoryDesktop")),
        KeyBinding::new("down", FocusNextItem, Some("CommandPalette")),
        KeyBinding::new("up", FocusPreviousItem, Some("CommandPalette")),
    ]);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Library,
    Review,
    Relations,
    Clients,
    Health,
}

impl View {
    fn from_name(name: &str) -> Self {
        match name {
            "Review" => Self::Review,
            "Relations" => Self::Relations,
            "Clients" => Self::Clients,
            "Health" => Self::Health,
            _ => Self::Library,
        }
    }
    fn title(self) -> &'static str {
        match self {
            Self::Library => "Library",
            Self::Review => "Review",
            Self::Relations => "Relations",
            Self::Clients => "Clients",
            Self::Health => "Health",
        }
    }
    fn icon(self) -> IconName {
        match self {
            Self::Library => IconName::BookOpen,
            Self::Review => IconName::Inbox,
            Self::Relations => IconName::Network,
            Self::Clients => IconName::Key,
            Self::Health => IconName::Activity,
        }
    }
}

pub struct Workbench {
    root: DataRoot,
    cli: Option<PathBuf>,
    service: Service,
    preferences: Preferences,
    draft: Option<EditDraft>,
    focus: FocusHandle,
    list_focus: FocusHandle,
    palette_focus: FocusHandle,
    projects: Vec<ProjectInfo>,
    project: Option<String>,
    health: Value,
    view: View,
    query: Entity<InputState>,
    project_filter: Entity<InputState>,
    editor: Entity<TextareaState>,
    target_editor: Entity<TextareaState>,
    status: Option<String>,
    page: Page,
    cursor: Option<String>,
    previous_cursors: Vec<Option<String>>,
    candidates: Vec<Value>,
    selected_candidate: Option<Value>,
    selected: Option<Memory>,
    graph: Value,
    graph_layout: Graph,
    graph_zoom: f32,
    graph_scroll: ScrollHandle,
    graph_drag: Option<Point<Pixels>>,
    operations: Value,
    grants: Vec<Value>,
    loading: bool,
    connected: bool,
    changing: bool,
    starting: bool,
    editing: bool,
    palette: bool,
    notice: String,
    error: Option<Failure>,
    generation: Generation,
    detail_generation: Generation,
    event_generation: u64,
    event_instance: Option<String>,
    polling: bool,
    list_scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
    load_task: Option<Task<()>>,
    detail_task: Option<Task<()>>,
    debounce: Option<Task<()>>,
    _poll: Option<Task<()>>,
}

impl Workbench {
    pub fn new(
        root: DataRoot,
        cli: Option<PathBuf>,
        service: Service,
        preferences: Preferences,
        settings_error: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search project knowledge…"));
        let project_filter =
            cx.new(|cx| InputState::new(window, cx).placeholder("Find a project…"));
        let editor = cx.new(|cx| TextareaState::new(window, cx));
        let target_editor = cx.new(|cx| TextareaState::new(window, cx));
        let subscriptions = vec![
            cx.subscribe_in(&query, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::Change) && !this.editing {
                    this.generation.advance();
                    this.cursor = None;
                    this.previous_cursors.clear();
                    this.debounce = Some(cx.spawn_in(window, async move |this, cx| {
                        cx.background_executor()
                            .timer(Duration::from_millis(220))
                            .await;
                        let _ = this.update_in(cx, |this, window, cx| this.load(window, cx));
                    }));
                }
            }),
            cx.subscribe_in(&project_filter, window, |_, _, _: &InputEvent, _, cx| {
                cx.notify()
            }),
            cx.subscribe_in(
                &editor,
                window,
                |this, editor, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::Change)
                        && let Some(draft) = this.draft.as_mut()
                    {
                        draft.text = editor.read(cx).value().to_string();
                    }
                },
            ),
        ];
        let mut this = Self {
            project: preferences.project.clone(),
            view: View::from_name(&preferences.view),
            status: preferences.status.clone(),
            graph_zoom: preferences.graph_zoom,
            preferences,
            draft: None,
            root,
            cli,
            service,
            focus: cx.focus_handle(),
            list_focus: cx.focus_handle(),
            palette_focus: cx.focus_handle(),
            projects: vec![],
            health: Value::Null,
            query,
            project_filter,
            editor,
            target_editor,
            page: Page::default(),
            cursor: None,
            previous_cursors: vec![],
            candidates: vec![],
            selected_candidate: None,
            selected: None,
            graph: Value::Null,
            graph_layout: Graph::default(),
            graph_scroll: ScrollHandle::new(),
            graph_drag: None,
            operations: Value::Null,
            grants: vec![],
            loading: true,
            connected: false,
            changing: false,
            starting: false,
            editing: false,
            palette: false,
            notice: settings_error
                .map(|e| format!("Saved settings unavailable: {e}"))
                .unwrap_or_default(),
            error: None,
            generation: Generation::default(),
            detail_generation: Generation::default(),
            event_generation: 0,
            event_instance: None,
            polling: false,
            list_scroll: UniformListScrollHandle::new(),
            _subscriptions: subscriptions,
            load_task: None,
            detail_task: None,
            debounce: None,
            _poll: None,
        };
        this.overview(window, cx);
        // Application shortcuts (Ctrl/Cmd+K, F, R) dispatch through the focused
        // element, so the workbench holds focus from the first frame.
        this.focus_main(window, cx);
        let owner = cx.entity().downgrade();
        this._subscriptions
            .push(window.observe_window_appearance(move |window, cx| {
                let _ = owner.update(cx, |this, cx| {
                    if this.preferences.appearance == Appearance::System {
                        Theme::change(ThemeMode::from(window.appearance()), Some(window), cx);
                        ui::apply_platform_font(cx);
                    }
                });
            }));
        this._poll = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(3)).await;
                if this
                    .update_in(cx, |this, window, cx| this.poll(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
        this
    }

    fn ask(
        &mut self,
        command: Command,
        window: &mut Window,
        cx: &mut Context<Self>,
        apply: impl FnOnce(&mut Self, Value, &mut Window, &mut Context<Self>) + 'static,
    ) -> Option<Task<()>> {
        let mutation = matches!(&command, Command::Change(_));
        let receiver = match self.service.request(command) {
            Ok(receiver) => receiver,
            Err(error) => {
                if mutation {
                    self.changing = false;
                }
                self.failed(error, cx);
                return None;
            }
        };
        Some(cx.spawn_in(window, async move |this, cx| {
            let result = receiver.await.unwrap_or_else(|_| {
                Err(Failure {
                    code: "UNAVAILABLE".into(),
                    message: "The desktop connection closed. Refresh to reconnect.".into(),
                    uncertain: false,
                })
            });
            let _ = this.update_in(cx, move |this, window, cx| {
                match result {
                    Ok(value) => apply(this, value, window, cx),
                    Err(error) => {
                        if mutation {
                            this.changing = false;
                        }
                        this.failed(error, cx);
                    }
                }
                cx.notify();
            });
        }))
    }

    fn failed(&mut self, error: Failure, cx: &mut Context<Self>) {
        if matches!(error.code.as_str(), "UNAVAILABLE" | "TIMEOUT") {
            self.connected = false;
        }
        self.loading = false;
        self.polling = false;
        self.error = Some(error);
        cx.notify();
    }

    fn persist(&mut self, cx: &mut Context<Self>) {
        self.preferences.project = self.project.clone();
        self.preferences.view = self.view.title().into();
        self.preferences.status = self.status.clone();
        self.preferences.graph_zoom = self.graph_zoom;
        match self
            .service
            .request(Command::SavePreferences(self.preferences.clone()))
        {
            Ok(reply) => cx
                .spawn(async move |this, cx| {
                    if let Ok(Err(error)) = reply.await {
                        let _ = this.update(cx, |this, cx| {
                            this.notice = format!("Settings not saved: {error}");
                            cx.notify();
                        });
                    }
                })
                .detach(),
            Err(error) => {
                self.notice = format!("Settings not saved: {error}");
                cx.notify();
            }
        }
    }

    fn fetch_draft_base(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = self.draft.as_ref() else {
            return;
        };
        let project = draft.project.clone();
        let id = draft.base.id.clone();
        let expected = project.clone();
        let expected_id = id.clone();
        if let Some(task) = self.ask(
            Command::Read(Read::Memory { project, id }),
            window,
            cx,
            move |this, value, window, cx| {
                let Some(draft) = this
                    .draft
                    .as_mut()
                    .filter(|d| d.project == expected && d.base.id == expected_id)
                else {
                    return;
                };
                match serde_json::from_value::<Memory>(value) {
                    Ok(memory) => {
                        if let Err(message) = draft.observe(memory.clone()) {
                            this.notice = message.into();
                            return;
                        }
                        this.target_editor
                            .update(cx, |state, cx| state.set_value(memory.text, window, cx));
                    }
                    Err(_) => {
                        this.notice =
                            "Invalid current revision reply; the draft is unchanged.".into()
                    }
                }
            },
        ) {
            task.detach();
        }
    }

    fn save_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = self.draft.as_mut() else {
            return;
        };
        draft.text = self.editor.read(cx).value().to_string();
        let change = Change::Edit {
            project: draft.project.clone(),
            id: draft.base.id.clone(),
            revision: draft.base.revision,
            text: draft.text.clone(),
        };
        self.mutate(change, window, cx);
    }

    fn overview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.polling = true;
        if let Some(task) = self.ask(
            Command::Read(Read::Overview),
            window,
            cx,
            |this, value, window, cx| {
                let was_disconnected = !this.connected;
                this.connected = true;
                this.polling = false;
                this.error = None;
                this.health = value["health"].clone();
                this.projects =match serde_json::from_value(value["projects"].clone()) {Ok(projects)=>projects,Err(_)=>{this.failed(Failure{code:"PROTOCOL".into(),message:"Invalid project registry reply. Previously loaded data is retained.".into(),uncertain:false},cx);return;}};
                if this.editing {
                    // Reconnect/discovery must never discard a protected draft,
                    // including when its project was detached by another client.
                    this.loading = false;
                    if this.current_project().is_none() {
                        this.notice = "The draft's project is no longer registered. Copy or cancel the draft before switching projects.".into();
                    }
                    return;
                }
                if this.project.is_none()
                    || !this
                        .projects
                        .iter()
                        .any(|p| Some(&p.project_id) == this.project.as_ref())
                {
                    this.project = this.projects.first().map(|p| p.project_id.clone());
                    this.reset();
                    this.load(window, cx);
                } else if was_disconnected {
                    this.load(window, cx);
                }
                this.loading = false;
            },
        ) {
            task.detach();
        }
    }

    fn reset(&mut self) {
        self.draft = None;
        self.generation.advance();
        self.detail_generation.advance();
        self.page = Page::default();
        self.cursor = None;
        self.previous_cursors.clear();
        self.selected = None;
        self.selected_candidate = None;
        self.candidates.clear();
        self.graph = Value::Null;
        self.operations = Value::Null;
        self.event_generation = 0;
        self.event_instance = None;
        self.error = None;
    }

    fn poll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.polling || self.changing {
            return;
        }
        if !self.connected {
            self.overview(window, cx);
            return;
        }
        let Some(project) = self.project.clone() else {
            return;
        };
        self.polling = true;
        let command = Read::Events {
            project: project.clone(),
            generation: self.event_generation,
            instance: self.event_instance.clone(),
        };
        if let Some(task) = self.ask(
            Command::Read(command),
            window,
            cx,
            move |this, value, window, cx| {
                this.polling = false;
                if this.project.as_ref() != Some(&project) {
                    return;
                }
                this.event_generation = value["generation"].as_u64().unwrap_or(0);
                this.event_instance = value["daemonInstanceId"].as_str().map(str::to_string);
                let changed = value["resync"].as_bool().unwrap_or(false)
                    || value["events"].as_array().is_some_and(|v| !v.is_empty());
                if changed && !this.editing && !this.loading {
                    this.load(window, cx);
                }
            },
        ) {
            task.detach();
        }
    }

    fn current_project(&self) -> Option<&ProjectInfo> {
        self.projects
            .iter()
            .find(|p| Some(&p.project_id) == self.project.as_ref())
    }

    fn writable(&self) -> bool {
        self.connected
            && !self.changing
            && self
                .current_project()
                .is_some_and(|p| p.admission == Admission::ReadWrite)
    }

    fn navigate(&mut self, view: View, window: &mut Window, cx: &mut Context<Self>) {
        if self.changing {
            self.notice = "Wait for the pending change before changing views.".into();
            cx.notify();
            return;
        }
        if self.editing {
            self.notice = "Save or cancel your draft before changing views.".into();
            cx.notify();
            return;
        }
        self.view = view;
        self.persist(cx);
        self.palette = false;
        self.reset();
        self.load(window, cx);
        self.focus_main(window, cx);
    }

    /// Keyboard focus for the working view: the record list in Library and
    /// Review, so Up/Down select immediately; the workbench elsewhere. Both
    /// sit inside the workbench, so application shortcuts keep working.
    fn focus_main(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.view, View::Library | View::Review) {
            self.list_focus.focus(window, cx);
        } else {
            self.focus.focus(window, cx);
        }
    }

    fn select_project(&mut self, project: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.changing {
            self.notice = "Wait for the pending change before switching projects.".into();
            cx.notify();
            return;
        }
        if self.editing {
            self.notice = "Save or cancel your draft before switching projects.".into();
            cx.notify();
            return;
        }
        self.project = Some(project);
        self.persist(cx);
        self.reset();
        self.load(window, cx);
    }

    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing {
            self.fetch_draft_base(window, cx);
            return;
        }
        let Some(project) = self.project.clone() else {
            self.loading = false;
            return;
        };
        let ticket = self.generation.advance();
        self.loading = true;
        self.error = None;
        let read = match self.view {
            View::Library => Read::Library(Query {
                project,
                text: self.query.read(cx).value().to_string(),
                status: self.status.clone(),
                cursor: self.cursor.clone(),
                ..Default::default()
            }),
            View::Review => Read::Review { project },
            View::Relations => Read::Graph {
                project,
                node: self.query.read(cx).value().to_string(),
            },
            View::Health => Read::Health { project },
            View::Clients => Read::Clients,
        };
        self.load_task = self.ask(
            Command::Read(read),
            window,
            cx,
            move |this, value, window, cx| {
                if !this.generation.accepts(ticket) {
                    return;
                }
                this.loading = false;
                match this.view {
                    View::Library => {
                        match serde_json::from_value::<Page>(value) {
                            Ok(page) => this.page = page,
                            Err(_) => {
                                this.failed(
                                    Failure {
                                        code: "PROTOCOL".into(),
                                        message: "Invalid library reply.".into(),
                                        uncertain: false,
                                    },
                                    cx,
                                );
                                return;
                            }
                        }
                        if this.selected.is_none()
                            && let Some(memory) = this.page.memories.first().cloned()
                        {
                            this.inspect(memory, window, cx);
                        }
                    }
                    View::Review => {
                        this.candidates =
                            value["candidates"].as_array().cloned().unwrap_or_default();
                        this.selected_candidate = None;
                    }
                    View::Relations => {
                        this.graph_layout = Graph::from_reply(&value);
                        this.graph = value;
                    }
                    View::Health => this.operations = value,
                    View::Clients => {
                        this.grants = value["grants"].as_array().cloned().unwrap_or_default()
                    }
                }
            },
        );
        cx.notify();
    }

    fn inspect(&mut self, memory: Memory, window: &mut Window, cx: &mut Context<Self>) {
        if self.changing {
            return;
        }
        if self.editing {
            self.notice = "Save or cancel your draft before selecting another memory.".into();
            return;
        }
        self.detail_generation.advance();
        self.draft = None;
        self.editor.update(cx, |state, cx| {
            state.set_value(memory.text.clone(), window, cx)
        });
        self.selected = Some(memory);
        self.selected_candidate = None;
        cx.notify();
    }

    fn inspect_id(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project.clone() else {
            return;
        };
        let ticket = self.detail_generation.advance();
        self.detail_task = self.ask(
            Command::Read(Read::Memory { project, id }),
            window,
            cx,
            move |this, value, window, cx| {
                if !this.detail_generation.accepts(ticket) {
                    return;
                }
                if let Ok(memory) = serde_json::from_value(value) {
                    this.inspect(memory, window, cx);
                }
            },
        );
    }

    fn select_candidate(&mut self, candidate: Value, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project.clone() else {
            return;
        };
        self.selected = None;
        self.editor.update(cx, |state, cx| {
            state.set_value(string(&candidate, "text"), window, cx)
        });
        self.target_editor.update(cx, |state, cx| {
            state.set_value("This proposal creates new project knowledge.", window, cx)
        });
        let target = string(&candidate, "targetMemoryId");
        self.selected_candidate = Some(candidate);
        let ticket = self.detail_generation.advance();
        if !target.is_empty() {
            self.detail_task = self.ask(
                Command::Read(Read::Memory {
                    project,
                    id: target,
                }),
                window,
                cx,
                move |this, value, window, cx| {
                    if !this.detail_generation.accepts(ticket) {
                        return;
                    }
                    if let Ok(memory) = serde_json::from_value::<Memory>(value) {
                        this.target_editor.update(cx, |state, cx| {
                            state.set_value(memory.text.clone(), window, cx)
                        });
                        this.selected = Some(memory);
                    }
                },
            );
        }
        cx.notify();
    }

    fn mutate(&mut self, change: Change, window: &mut Window, cx: &mut Context<Self>) {
        if self.changing {
            return;
        }
        self.changing = true;
        self.error = None;
        if let Some(task) = self.ask(
            Command::Change(change),
            window,
            cx,
            |this, _, window, cx| {
                this.changing = false;
                this.editing = false;
                this.draft = None;
                this.notice = "Change committed by the service.".into();
                let id = this.selected.as_ref().map(|m| m.id.clone());
                this.load(window, cx);
                if let Some(id) = id {
                    this.inspect_id(id, window, cx);
                }
            },
        ) {
            task.detach();
        }
        cx.notify();
    }

    fn confirm(
        &mut self,
        title: &'static str,
        description: String,
        change: Change,
        destructive: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let owner = cx.entity().downgrade();
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(if destructive {
                "Type CONFIRM to continue"
            } else {
                "Optional review note"
            })
        });
        window.open_dialog(cx, move |dialog, _, _| {
            let owner = owner.clone();
            let input = input.clone();
            let change = change.clone();
            dialog
                .title(title)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_4()
                        .child(description.clone())
                        .child(Input::new(&input)),
                )
                .on_ok(move |_, window, cx| {
                    let note = input.read(cx).value().to_string();
                    if destructive && note != "CONFIRM" {
                        return false;
                    }
                    let mut action = change.clone();
                    if let Change::Reject { reason, .. } = &mut action
                        && !note.trim().is_empty()
                    {
                        *reason = note;
                    }
                    let _ = owner.update(cx, |this, cx| this.mutate(action, window, cx));
                    true
                })
        });
    }

    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.starting {
            return;
        }
        let Some(cli) = self.cli.clone() else {
            return;
        };
        let root = self.root.clone();
        self.starting = true;
        self.notice = "Starting the shared service…".into();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { service::start_service(&cli, &root) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.starting = false;
                match result {
                    Ok(()) => {
                        this.notice = "Shared service started.".into();
                        this.overview(window, cx)
                    }
                    Err(message) => this.failed(
                        Failure {
                            code: "START_FAILED".into(),
                            message,
                            uncertain: false,
                        },
                        cx,
                    ),
                }
            });
        })
        .detach();
    }

    fn select_offset(&mut self, offset: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.view == View::Review {
            if self.candidates.is_empty() {
                return;
            }
            let current = self
                .selected_candidate
                .as_ref()
                .and_then(|c| self.candidates.iter().position(|r| r["id"] == c["id"]));
            let next = match current {
                Some(ix) => (ix as isize + offset).clamp(0, self.candidates.len() as isize - 1),
                None => 0,
            } as usize;
            let candidate = self.candidates[next].clone();
            self.select_candidate(candidate, window, cx);
            return;
        }
        if self.view != View::Library || self.page.memories.is_empty() {
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|m| self.page.memories.iter().position(|r| r.id == m.id))
            .unwrap_or(0);
        let next =
            (current as isize + offset).clamp(0, self.page.memories.len() as isize - 1) as usize;
        let memory = self.page.memories[next].clone();
        self.inspect(memory, window, cx);
        self.list_scroll
            .scroll_to_item(next, ScrollStrategy::Nearest);
    }
}

const VIEWS: [View; 5] = [
    View::Library,
    View::Review,
    View::Relations,
    View::Clients,
    View::Health,
];

impl Workbench {
    fn empty(&self, icon: IconName, title: &str, description: &str, cx: &App) -> AnyElement {
        empty_state(icon, title.to_string(), description.to_string(), cx)
    }

    /// One-line monospace path with a copy action; long paths truncate
    /// instead of wrapping at arbitrary characters.
    fn path_field(
        &self,
        id: &'static str,
        label: &'static str,
        path: String,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = cx.theme().clone();
        let copied = path.clone();
        div()
            .flex()
            .items_center()
            .gap_3()
            .min_w_0()
            .text_sm()
            .child(
                div()
                    .w(px(112.))
                    .flex_none()
                    .text_color(theme.muted_foreground)
                    .child(label),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .font_family(theme.mono_font_family.clone())
                    .truncate()
                    .child(path.clone()),
            )
            .child(
                Button::new(id)
                    .ghost()
                    .xsmall()
                    .icon(IconName::Copy)
                    .tooltip(format!("Copy {}", label.to_lowercase()))
                    .disabled(path.is_empty())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()));
                        this.notice = format!("{label} copied.");
                        cx.notify();
                    })),
            )
    }

    /// Open or close the command palette. Opening moves keyboard focus to its
    /// first item; closing returns it to the workbench.
    fn set_palette(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = open;
        if open {
            self.palette_focus.focus(window, cx);
        } else {
            self.focus_main(window, cx);
        }
        cx.notify();
    }

    fn toggle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mode = if cx.theme().is_dark() {
            ThemeMode::Light
        } else {
            ThemeMode::Dark
        };
        self.preferences.appearance = if mode == ThemeMode::Light {
            Appearance::Light
        } else {
            Appearance::Dark
        };
        Theme::change(mode, Some(window), cx);
        ui::apply_platform_font(cx);
        self.persist(cx);
    }

    fn sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let filter = self.project_filter.read(cx).value().to_lowercase();
        let theme = cx.theme().clone();
        let visible = self
            .projects
            .iter()
            .filter(|p| {
                p.display_name.to_lowercase().contains(&filter)
                    || p.root.to_lowercase().contains(&filter)
            })
            .cloned()
            .collect::<Vec<_>>();
        let list = if visible.is_empty() {
            div()
                .px_2()
                .py_3()
                .child(caption(
                    if self.projects.is_empty() {
                        "No registered projects yet."
                    } else {
                        "No project matches this filter."
                    },
                    cx,
                ))
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                .children(visible.into_iter().map(|p| {
                    let id = p.project_id.clone();
                    let active = self.project.as_ref() == Some(&id);
                    Button::new(SharedString::from(format!("project-{id}")))
                        .ghost()
                        .selected(active)
                        .tooltip(p.root.clone())
                        .relative()
                        .w_full()
                        .h_auto()
                        .min_w_0()
                        .pl_3()
                        .pr_2()
                        .py_2()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.select_project(id.clone(), window, cx)
                        }))
                        .when(active, |row| {
                            row.child(
                                div()
                                    .absolute()
                                    .left_0()
                                    .top(px(8.))
                                    .bottom(px(8.))
                                    .w(px(3.))
                                    .rounded_full()
                                    .bg(theme.primary),
                            )
                        })
                        .child(
                            div()
                                .flex_1()
                                .flex()
                                .items_center()
                                .gap_2()
                                .min_w_0()
                                .child(
                                    Icon::new(if active {
                                        IconName::FolderOpen
                                    } else {
                                        IconName::Folder
                                    })
                                    .size_4()
                                    .text_color(if active {
                                        theme.primary
                                    } else {
                                        theme.muted_foreground
                                    }),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .flex()
                                        .flex_col()
                                        .child(
                                            div()
                                                .text_sm()
                                                .font_weight(if active {
                                                    FontWeight::SEMIBOLD
                                                } else {
                                                    FontWeight::NORMAL
                                                })
                                                .text_color(theme.sidebar_foreground)
                                                .truncate()
                                                .child(p.display_name.clone()),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .truncate()
                                                .child(p.root.clone()),
                                        ),
                                ),
                        )
                }))
                .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                div()
                    .px_4()
                    .pt_4()
                    .pb_3()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .size_8()
                            .flex_none()
                            .rounded_lg()
                            .bg(theme.primary)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                Icon::new(IconName::Database)
                                    .size_4()
                                    .text_color(theme.primary_foreground),
                            ),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(theme.sidebar_foreground)
                                    .whitespace_nowrap()
                                    .child("WrongMemory"),
                            )
                            .child(caption("Project knowledge", cx)),
                    ),
            )
            .child(
                div().px_3().pb_2().child(
                    Input::new(&self.project_filter).small().prefix(
                        Icon::new(IconName::Search)
                            .size_3p5()
                            .text_color(theme.muted_foreground),
                    ),
                ),
            )
            .child(
                div()
                    .px_4()
                    .pt_2()
                    .pb_1()
                    .flex()
                    .justify_between()
                    .child(section("PROJECTS", cx))
                    .child(caption(self.projects.len().to_string(), cx)),
            )
            .child(
                div()
                    .id("projects")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_2()
                    .child(list),
            )
            .child(
                div()
                    .p_2()
                    .border_t_1()
                    .border_color(theme.sidebar_border)
                    .child(
                        Button::new("palette-entry")
                            .ghost()
                            .w_full()
                            .px_2()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.set_palette(!this.palette, window, cx)
                            }))
                            .child(
                                Icon::new(IconName::Command)
                                    .size_4()
                                    .text_color(theme.muted_foreground),
                            )
                            .child(div().flex_1().text_sm().child("Commands"))
                            .child(kbd("k")),
                    ),
            )
            .into_any_element()
    }

    fn toolbar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let project = self.current_project().cloned();
        let pending = self.candidates.len();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .px_5()
            .pt_4()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_xl()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(
                                        project
                                            .as_ref()
                                            .map(|p| p.display_name.clone())
                                            .unwrap_or_else(|| "No project selected".into()),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .truncate()
                                    .child(
                                        project.as_ref().map(|p| p.root.clone()).unwrap_or_else(
                                            || "Select or register a project to begin.".into(),
                                        ),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap_1()
                            .when(
                                self.connected && project.is_some() && !self.writable(),
                                |bar| bar.child(chip("Read-only", Tone::Warning)),
                            )
                            .child(
                                Button::new("refresh")
                                    .ghost()
                                    .small()
                                    .icon(IconName::RefreshCw)
                                    .label("Refresh")
                                    .loading(self.loading)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.overview(window, cx);
                                        this.load(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("theme")
                                    .ghost()
                                    .small()
                                    .icon(if theme.is_dark() {
                                        IconName::Sun
                                    } else {
                                        IconName::Moon
                                    })
                                    .tooltip(if theme.is_dark() {
                                        "Switch to light appearance"
                                    } else {
                                        "Switch to dark appearance"
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.toggle_theme(window, cx)
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap_1()
                    .children(VIEWS.into_iter().map(|view| {
                        let selected = self.view == view;
                        Button::new(SharedString::from(format!("view-{}", view.title())))
                            .ghost()
                            .h_auto()
                            .px_3()
                            .pb_2()
                            .pt_1()
                            .rounded_none()
                            .border_b_2()
                            .border_color(if selected {
                                theme.primary
                            } else {
                                transparent_black()
                            })
                            .text_sm()
                            .text_color(if selected {
                                theme.foreground
                            } else {
                                theme.muted_foreground
                            })
                            .when(selected, |t| t.font_weight(FontWeight::MEDIUM))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.navigate(view, window, cx)
                            }))
                            .child(Icon::new(view.icon()).size_4())
                            .child(view.title())
                            .when(view == View::Review && pending > 0, |t| {
                                t.child(
                                    div()
                                        .px_1p5()
                                        .rounded_full()
                                        .bg(theme.primary)
                                        .text_color(theme.primary_foreground)
                                        .text_xs()
                                        .child(pending.to_string()),
                                )
                            })
                    })),
            )
            .into_any_element()
    }

    fn library(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let statuses = [
            ("Active", Some("active")),
            ("All", None),
            ("Stale", Some("stale")),
            ("Archived", Some("archived")),
            ("Deleted", Some("deleted")),
        ];
        let theme = cx.theme().clone();
        let count = self.page.memories.len();
        let filtered = !self.query.read(cx).value().trim().is_empty()
            || self.status.as_deref() != Some("active");
        let list = if count == 0 {
            if self.loading {
                self.empty(
                    IconName::LoaderCircle,
                    "Loading project knowledge",
                    "Reading the current page from the shared service.",
                    cx,
                )
            } else if filtered {
                self.empty(
                    IconName::Search,
                    "No matching memories",
                    "Try another query or status filter.",
                    cx,
                )
            } else {
                self.empty(IconName::BookOpen, "No knowledge yet", "This project has no active memories. Records are never fabricated for an empty project.", cx)
            }
        } else {
            uniform_list(
                "memory-list",
                count,
                cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                    let theme = cx.theme().clone();
                    range
                        .map(|ix| {
                            let memory = this.page.memories[ix].clone();
                            let selected =
                                this.selected.as_ref().is_some_and(|m| m.id == memory.id);
                            let anchor = memory
                                .anchors
                                .first()
                                .map(|a| string(a, "path"))
                                .unwrap_or_default();
                            let label = format!(
                                "{} memory, revision {}: {}",
                                memory.kind,
                                memory.revision,
                                preview(&memory.text, 160)
                            );
                            let status = memory.status.clone();
                            let kind = memory.kind.replace('_', " ");
                            let revision = memory.revision;
                            let text = preview(&memory.text, 150);
                            div()
                                .id(("memory", ix))
                                .role(Role::Button)
                                .aria_label(label)
                                .h(px(104.))
                                .w_full()
                                .relative()
                                .px_4()
                                .py_3()
                                .cursor_pointer()
                                .border_b_1()
                                .border_color(theme.border.opacity(0.6))
                                .when(selected, |row| row.bg(theme.list_active))
                                .when(!selected, |row| row.hover(|s| s.bg(theme.list_hover)))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.list_focus.focus(window, cx);
                                    this.inspect(memory.clone(), window, cx);
                                }))
                                .when(selected, |row| {
                                    row.child(
                                        div()
                                            .absolute()
                                            .left_0()
                                            .top_0()
                                            .bottom_0()
                                            .w(px(3.))
                                            .bg(theme.primary),
                                    )
                                })
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap_1p5()
                                        .overflow_hidden()
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_2()
                                                .text_xs()
                                                .child(
                                                    div()
                                                        .size(px(7.))
                                                        .rounded_full()
                                                        .bg(Tone::for_status(&status).color(cx)),
                                                )
                                                .child(
                                                    div()
                                                        .font_weight(FontWeight::MEDIUM)
                                                        .text_color(theme.foreground)
                                                        .child(kind),
                                                )
                                                .child(
                                                    div()
                                                        .text_color(theme.muted_foreground)
                                                        .child(format!("r{revision}")),
                                                )
                                                .when(status != "active", |r| {
                                                    r.child(chip(
                                                        status.clone(),
                                                        Tone::for_status(&status),
                                                    ))
                                                }),
                                        )
                                        .child(
                                            div()
                                                .text_sm()
                                                .line_height(relative(1.45))
                                                .line_clamp(2)
                                                .text_ellipsis()
                                                .child(text),
                                        )
                                        .when(!anchor.is_empty(), |c| {
                                            c.child(
                                                div()
                                                    .flex()
                                                    .items_center()
                                                    .gap_1()
                                                    .text_xs()
                                                    .text_color(theme.muted_foreground)
                                                    .child(Icon::new(IconName::FileText).size_3())
                                                    .child(div().truncate().child(anchor)),
                                            )
                                        }),
                                )
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.list_scroll)
            .flex_1()
            .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .key_context("MemoryLibrary")
            .track_focus(&self.list_focus)
            .tab_stop(true)
            .child(
                div()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        Input::new(&self.query).disabled(self.editing).prefix(
                            Icon::new(IconName::Search)
                                .size_4()
                                .text_color(theme.muted_foreground),
                        ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .child(div().flex().flex_wrap().gap_1().children(
                                statuses.into_iter().map(|(label, status)| {
                                    Button::new(label)
                                        .ghost()
                                        .xsmall()
                                        .label(label)
                                        .selected(self.status.as_deref() == status)
                                        .disabled(self.editing)
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.status = status.map(str::to_string);
                                            this.persist(cx);
                                            this.cursor = None;
                                            this.previous_cursors.clear();
                                            this.load(window, cx);
                                        }))
                                }),
                            ))
                            .child(caption(
                                if self.loading {
                                    "Loading…".into()
                                } else {
                                    format!("{} total", self.page.total)
                                },
                                cx,
                            )),
                    ),
            )
            .child(list)
            .child(
                div()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        Button::new("previous-page")
                            .ghost()
                            .xsmall()
                            .icon(IconName::ChevronLeft)
                            .label("Previous")
                            .disabled(
                                self.previous_cursors.is_empty() || self.loading || self.editing,
                            )
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.cursor = this.previous_cursors.pop().flatten();
                                this.load(window, cx);
                            })),
                    )
                    .child(caption(
                        format!("Page {} · {} shown", self.previous_cursors.len() + 1, count),
                        cx,
                    ))
                    .child(
                        Button::new("next-page")
                            .ghost()
                            .xsmall()
                            .label("Next")
                            .icon(IconName::ChevronRight)
                            .disabled(
                                self.page.next_cursor.is_none() || self.loading || self.editing,
                            )
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.previous_cursors.push(this.cursor.clone());
                                this.cursor = this.page.next_cursor.clone();
                                this.load(window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    fn inspector(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(memory) = self.selected.clone() else {
            return self.empty(
                IconName::Inspector,
                "Evidence inspector",
                "Select a memory to inspect its full text, source anchors and current revision.",
                cx,
            );
        };
        let theme = cx.theme().clone();
        let project = self.project.clone().unwrap_or_default();
        let mut toolbar = div().flex().flex_wrap().items_center().gap_2();
        if self.editing {
            toolbar = toolbar
                .child(
                    Button::new("save-draft")
                        .primary()
                        .small()
                        .icon(IconName::Check)
                        .label("Save revision")
                        .loading(self.changing)
                        .disabled(!self.writable())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.save_draft(window, cx);
                        })),
                )
                .child(
                    Button::new("cancel-draft")
                        .small()
                        .label("Cancel")
                        .disabled(self.changing)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.editing = false;
                            if let Some(m) = this
                                .draft
                                .take()
                                .and_then(|d| d.server)
                                .or_else(|| this.selected.clone())
                            {
                                this.inspect(m, window, cx);
                            }
                        })),
                )
                .child(
                    Button::new("copy-draft")
                        .ghost()
                        .small()
                        .icon(IconName::Copy)
                        .label("Copy draft")
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(
                                this.editor.read(cx).value().to_string(),
                            ));
                            this.notice = "Draft text copied.".into();
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("compare-server-revision")
                        .ghost()
                        .small()
                        .icon(IconName::Replace)
                        .label("Compare current revision")
                        .disabled(self.changing)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.fetch_draft_base(window, cx)),
                        ),
                );
            if let Some(server) = self.draft.as_ref().and_then(|d| d.server.as_ref()) {
                toolbar = toolbar.child(
                    Button::new("rebase-draft")
                        .small()
                        .label(format!("Use revision {} for next save", server.revision))
                        .disabled(self.changing)
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(draft) = this.draft.as_mut() {
                                match draft.rebase() {
                                    Ok(revision) => {
                                        this.selected = Some(draft.base.clone());
                                        this.error = None;
                                        this.notice = format!("Draft retained. The next save is guarded by revision {revision}.");
                                    }
                                    Err(message) => this.notice = message.into(),
                                }
                            }
                            cx.notify();
                        })),
                );
            }
        } else {
            toolbar = toolbar
                .child(
                    Button::new("edit-memory")
                        .primary()
                        .small()
                        .icon(IconName::Pencil)
                        .label("Edit")
                        .disabled(!self.writable())
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let (Some(project), Some(memory)) =
                                (this.project.clone(), this.selected.clone())
                            {
                                this.draft = Some(EditDraft::new(project, memory));
                            }
                            this.editing = true;
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("copy-memory")
                        .ghost()
                        .small()
                        .icon(IconName::Copy)
                        .label("Copy text")
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(m) = &this.selected {
                                cx.write_to_clipboard(ClipboardItem::new_string(m.text.clone()));
                                this.notice = "Memory text copied.".into();
                                cx.notify();
                            }
                        })),
                );
            if memory.status == "deleted" || memory.status == "archived" || memory.status == "stale"
            {
                let id = memory.id.clone();
                let pid = project.clone();
                toolbar = toolbar.child(
                    Button::new("recover-memory")
                        .small()
                        .icon(IconName::Undo2)
                        .label("Recover")
                        .disabled(!self.writable())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.confirm(
                                "Recover memory",
                                "The service will validate the current lifecycle and successor chain.".into(),
                                Change::Recover { project: pid.clone(), id: id.clone() },
                                false,
                                window,
                                cx,
                            )
                        })),
                );
            }
            if memory.status != "deleted" {
                let id = memory.id.clone();
                let pid = project.clone();
                toolbar = toolbar.child(div().flex_1()).child(
                    Button::new("delete-memory")
                        .danger()
                        .ghost()
                        .small()
                        .icon(IconName::Delete)
                        .label("Delete")
                        .disabled(!self.writable())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.confirm(
                                "Delete memory",
                                "Creates a recoverable tombstone. This also authorizes the force override for a permanent record. Type CONFIRM to continue.".into(),
                                Change::Delete {
                                    project: pid.clone(),
                                    id: id.clone(),
                                    reason: "Deleted by the native desktop operator".into(),
                                },
                                true,
                                window,
                                cx,
                            )
                        })),
                );
            }
        }
        let anchors = memory
            .anchors
            .iter()
            .map(|a| {
                let symbol = string(a, "symbol");
                let path = string(a, "path");
                let target = match (path.is_empty(), symbol.is_empty()) {
                    (false, true) => path,
                    (false, false) => format!("{path} #{symbol}"),
                    (true, false) => format!("#{symbol}"),
                    (true, true) => serde_json::to_string(a).unwrap_or_default(),
                };
                (string(a, "type"), target)
            })
            .collect::<Vec<_>>();
        let date = |key: &str| {
            memory.extra.get(key).and_then(Value::as_str).map(|s| {
                s.replace('T', " ")
                    .trim_end_matches('Z')
                    .split('.')
                    .next()
                    .unwrap_or_default()
                    .to_string()
            })
        };
        let mut details: Vec<(String, String)> = Vec::new();
        if let Some(scope) = memory.extra.get("scope").and_then(Value::as_str) {
            details.push(("Scope".into(), scope.into()));
        }
        for (key, label) in [
            ("importance", "Importance"),
            ("confidence", "Confidence"),
            ("freshness", "Freshness"),
        ] {
            if let Some(v) = memory.extra.get(key).and_then(Value::as_f64) {
                details.push((label.into(), format!("{v:.2}")));
            }
        }
        for (key, label) in [
            ("createdAt", "Created"),
            ("updatedAt", "Updated"),
            ("lastVerifiedAt", "Last verified"),
        ] {
            if let Some(v) = date(key) {
                details.push((label.into(), format!("{v} UTC")));
            }
        }
        if let Some(by) = memory.extra.get("wrongmemory") {
            let principal = string(by, "principalId");
            let client = string(by, "client");
            if !principal.is_empty() || !client.is_empty() {
                details.push((
                    "Recorded by".into(),
                    [principal, client]
                        .into_iter()
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join(" · "),
                ));
            }
        }
        let validity = memory.extra.get("validity").cloned().unwrap_or(Value::Null);
        let block = |title: &'static str, icon: IconName, body: AnyElement, cx: &App| {
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            Icon::new(icon)
                                .size_3p5()
                                .text_color(cx.theme().muted_foreground),
                        )
                        .child(section(title, cx)),
                )
                .child(body)
        };
        let anchors_body = if anchors.is_empty() {
            caption("No structural anchors.", cx).into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .children(anchors.into_iter().map(|(kind, target)| {
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .min_w_0()
                        .child(chip(kind, Tone::Neutral))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_sm()
                                .font_family(theme.mono_font_family.clone())
                                .truncate()
                                .child(target),
                        )
                }))
                .into_any_element()
        };
        let provenance_body = if memory.sources.is_empty() {
            caption("No recorded source.", cx).into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .gap_2()
                .children(memory.sources.iter().map(|s| {
                    div()
                        .p_2()
                        .rounded_md()
                        .bg(theme.muted.opacity(0.6))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .children(rows(s).into_iter().map(|(k, v)| field(k, v, cx)))
                }))
                .into_any_element()
        };
        let applicability_body = if validity.is_null() {
            caption(
                "No explicit applicability checks. Source presence is not a truth guarantee.",
                cx,
            )
            .into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .children(rows(&validity).into_iter().map(|(k, v)| field(k, v, cx)))
                .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .px_5()
                    .pt_4()
                    .pb_3()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .child(section(if self.editing { "EDIT DRAFT" } else { "EVIDENCE INSPECTOR" }, cx))
                            .child(caption(format!("Revision {}", memory.revision), cx)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_1p5()
                            .child(chip(memory.kind.replace('_', " "), Tone::Info))
                            .child(chip(memory.status.clone(), Tone::for_status(&memory.status)))
                            .children(memory.tags.iter().take(8).map(|tag| chip(format!("#{tag}"), Tone::Neutral))),
                    )
                    .child(toolbar),
            )
            .child(
                div()
                    .id("inspector-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_5()
                    .py_4()
                    .flex()
                    .flex_col()
                    .gap_5()
                    .child(div().h(px(if self.editing { 260. } else { 200. })).flex_none().child(Textarea::new(&self.editor).readonly(!self.editing).size_full()))
                    .when(self.editing && self.draft.as_ref().is_some_and(|d| d.server.is_some()), |pane| {
                        pane.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(banner(Tone::Warning, "Current server text", "Your draft above is unchanged. Compare, then choose the revision for the next save.", cx).mx_0().mt_0())
                                .child(div().h(px(150.)).flex_none().child(Textarea::new(&self.target_editor).readonly(true).size_full())),
                        )
                    })
                    .child(block("ANCHORS", IconName::Anchor, anchors_body, cx))
                    .child(block("PROVENANCE", IconName::FileClock, provenance_body, cx))
                    .child(block("APPLICABILITY", IconName::ShieldCheck, applicability_body, cx))
                    .when(!details.is_empty(), |pane| {
                        pane.child(block(
                            "RECORD",
                            IconName::Info,
                            div().flex().flex_col().gap_1().children(details.into_iter().map(|(k, v)| field(k, v, cx))).into_any_element(),
                            cx,
                        ))
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(caption("ID", cx))
                            .child(div().text_xs().font_family(theme.mono_font_family.clone()).text_color(theme.muted_foreground).child(memory.id.clone())),
                    ),
            )
            .into_any_element()
    }

    fn review(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.candidates.clone();
        let theme = cx.theme().clone();
        if rows.is_empty() {
            return self.empty(IconName::Inbox, "Review queue is empty", "Proposals from coding tools appear here for an explicit accept or reject decision.", cx);
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(caption(
                        format!("{} pending proposals · select one to compare", rows.len()),
                        cx,
                    )),
            )
            .key_context("MemoryLibrary")
            .track_focus(&self.list_focus)
            .child(
                div()
                    .id("review-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(rows.into_iter().enumerate().map(|(ix, c)| {
                        let selected = self
                            .selected_candidate
                            .as_ref()
                            .is_some_and(|s| s["id"] == c["id"]);
                        let item = c.clone();
                        let target = string(&c, "targetMemoryId");
                        div()
                            .id(("proposal", ix))
                            .role(Role::Button)
                            .aria_label(format!("Proposal: {}", preview(&string(&c, "text"), 120)))
                            .p_3()
                            .rounded_lg()
                            .border_1()
                            .border_color(if selected {
                                theme.primary
                            } else {
                                theme.border
                            })
                            .bg(if selected {
                                theme.list_active
                            } else {
                                theme.background
                            })
                            .when(!selected, |r| r.hover(|s| s.bg(theme.list_hover)))
                            .cursor_pointer()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.select_candidate(item.clone(), window, cx)
                            }))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(chip(string(&c, "kind").replace('_', " "), Tone::Info))
                                    .child(chip(
                                        if target.is_empty() {
                                            "New knowledge".to_string()
                                        } else {
                                            "Revision".to_string()
                                        },
                                        if target.is_empty() {
                                            Tone::Success
                                        } else {
                                            Tone::Warning
                                        },
                                    )),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .line_height(relative(1.45))
                                    .child(preview(&string(&c, "text"), 240)),
                            )
                            .when(!target.is_empty(), |r| {
                                r.child(caption(
                                    format!("Revises {} · r{}", target, c["targetRevision"]),
                                    cx,
                                ))
                            })
                    })),
            )
            .into_any_element()
    }

    fn comparison(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(candidate) = self.selected_candidate.clone() else {
            return self.empty(IconName::Scale, "Review with evidence", "Select a proposal. Its evidence and the current target are shown before any decision.", cx);
        };
        let id = string(&candidate, "id");
        let project = self.project.clone().unwrap_or_default();
        let conflict = self.selected.as_ref().is_some_and(|m| {
            candidate["targetRevision"]
                .as_i64()
                .is_some_and(|r| r != m.revision)
        });
        let has_target = !string(&candidate, "targetMemoryId").is_empty();
        let can_decide = self.writable() && !conflict;
        let mut actions = div().flex().flex_wrap().gap_2();
        if string(&candidate, "kind") == "memory_review" {
            for (label, decision) in [
                ("Keep", "keep"),
                ("Archive", "archive"),
                ("Delete", "delete"),
            ] {
                let id = id.clone();
                let pid = project.clone();
                let mut button = Button::new(label)
                    .small()
                    .label(label)
                    .disabled(!can_decide);
                if decision == "delete" {
                    button = button.danger();
                }
                actions = actions.child(button.on_click(cx.listener(move |this, _, window, cx| {
                    this.confirm(
                        "Resolve review",
                        format!("Apply {decision} to the revision-bound target. The service revalidates it before writing."),
                        Change::Resolve { project: pid.clone(), candidate: id.clone(), decision: decision.into() },
                        decision == "delete",
                        window,
                        cx,
                    )
                })));
            }
        } else {
            let id = id.clone();
            let pid = project.clone();
            actions = actions.child(
                Button::new("accept-proposal")
                    .primary()
                    .small()
                    .icon(IconName::Check)
                    .label("Accept proposal")
                    .disabled(!can_decide)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.confirm(
                            "Accept proposal",
                            "Commit this proposal as authoritative project knowledge. The service checks the target revision.".into(),
                            Change::Accept { project: pid.clone(), candidate: id.clone() },
                            false,
                            window,
                            cx,
                        )
                    })),
            );
        }
        actions = actions.child(
            Button::new("reject-proposal")
                .small()
                .icon(IconName::Close)
                .label("Reject")
                .disabled(!self.writable())
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.confirm(
                        "Reject proposal",
                        "The proposal remains in the resolved review history.".into(),
                        Change::Reject {
                            project: project.clone(),
                            candidate: id.clone(),
                            reason: "Rejected by the native desktop operator".into(),
                        },
                        false,
                        window,
                        cx,
                    )
                })),
        );
        let evidence = candidate
            .get("evidence")
            .or_else(|| candidate.get("reviewReason"))
            .cloned()
            .unwrap_or(Value::Null);
        div()
            .id("comparison-scroll")
            .size_full()
            .overflow_y_scroll()
            .p_5()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(section("REVIEW WORKBENCH", cx))
                    .child(chip(string(&candidate, "kind").replace('_', " "), Tone::Info)),
            )
            .when(conflict, |pane| pane.child(banner(Tone::Danger, "Revision conflict", "The target changed after this proposal was made. Refresh the proposal before deciding.", cx).mx_0().mt_0()))
            .child(actions)
            .when(has_target, |pane| {
                pane.child(section("CURRENT TARGET", cx))
                    .child(div().h(px(170.)).flex_none().child(Textarea::new(&self.target_editor).readonly(true).size_full()))
            })
            .child(section(if has_target { "PROPOSED REVISION" } else { "PROPOSED KNOWLEDGE" }, cx))
            .child(div().h(px(200.)).flex_none().child(Textarea::new(&self.editor).readonly(true).size_full()))
            .child(section("EVIDENCE", cx))
            .child(if evidence.is_null() {
                caption("No evidence was attached. Inspect the proposal's recorded sources before accepting.", cx).into_any_element()
            } else {
                div().flex().flex_col().gap_1().children(rows(&evidence).into_iter().map(|(k, v)| field(k, v, cx))).into_any_element()
            })
            .into_any_element()
    }

    fn health_view(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let now = now_ms();
        let stats = self.operations["stats"].clone();
        let schedule = self.operations["maintenance"]["schedule"].clone();
        let project = self.current_project().cloned();
        let card = |cx: &App| {
            div()
                .p_4()
                .rounded_lg()
                .border_1()
                .border_color(cx.theme().border)
                .flex()
                .flex_col()
                .gap_3()
        };
        let mut content = div()
            .id("health-content")
            .size_full()
            .overflow_y_scroll()
            .p_5()
            .flex()
            .flex_col()
            .gap_4();
        let store_path: String = project
            .as_ref()
            .map(|p| p.store_path.clone())
            .unwrap_or_default();
        content =
            content.child(
                card(cx)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Icon::new(IconName::Database)
                                    .size_4()
                                    .text_color(theme.muted_foreground),
                            )
                            .child(section("PROJECT STORAGE", cx)),
                    )
                    .child(field(
                        "Profile",
                        project
                            .as_ref()
                            .and_then(|p| p.storage_profile_id.clone())
                            .unwrap_or_else(|| "Unknown profile".into()),
                        cx,
                    ))
                    .child(field(
                        "Admission",
                        project
                            .as_ref()
                            .map(|p| p.admission_reason.clone())
                            .unwrap_or_default(),
                        cx,
                    ))
                    .child(self.path_field("store-path", "Store", store_path, cx))
                    .child(
                        div().flex().flex_wrap().gap_1p5().children(
                            stats["byStatus"].as_object().into_iter().flatten().map(
                                |(status, n)| {
                                    let count = n.as_i64().unwrap_or_default();
                                    chip(
                                        format!("{status} {count}"),
                                        if count == 0 {
                                            Tone::Neutral
                                        } else {
                                            Tone::for_status(status)
                                        },
                                    )
                                },
                            ),
                        ),
                    ),
            );
        let mut maintenance = card(cx).child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    Icon::new(IconName::Wrench)
                        .size_4()
                        .text_color(theme.muted_foreground),
                )
                .child(section("MAINTENANCE", cx)),
        );
        if schedule.is_null() {
            maintenance = maintenance.child(caption("No recorded schedule.", cx));
        } else {
            maintenance = maintenance
                .child(field("Enabled", scalar(&schedule["enabled"]), cx))
                .child(field(
                    "Cadence",
                    schedule["cadenceMs"]
                        .as_i64()
                        .map(|ms| format!("every {}", duration(ms)))
                        .unwrap_or_else(|| "—".into()),
                    cx,
                ))
                .child(field(
                    "Last run",
                    schedule["lastRunMs"]
                        .as_i64()
                        .map(|ms| relative_time(ms, now))
                        .unwrap_or_else(|| "Never".into()),
                    cx,
                ))
                .child(field(
                    "Next due",
                    schedule["nextDueMs"]
                        .as_i64()
                        .map(|ms| relative_time(ms, now))
                        .unwrap_or_else(|| "—".into()),
                    cx,
                ))
                .when(!string(&schedule, "lastOutcome").is_empty(), |c| {
                    let outcome = string(&schedule, "lastOutcome");
                    c.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .text_sm()
                            .child(
                                div()
                                    .w(px(112.))
                                    .flex_none()
                                    .text_color(theme.muted_foreground)
                                    .child("Last outcome"),
                            )
                            .child(chip(outcome.clone(), Tone::for_job(&outcome))),
                    )
                });
        }
        maintenance = maintenance.child(
            div().flex().child(
                Button::new("run-maintenance")
                    .small()
                    .icon(IconName::Play)
                    .label("Queue maintenance")
                    .disabled(!self.writable())
                    .on_click(cx.listener(|this, _, window, cx| {
                        if let Some(project) = this.project.clone() {
                            this.confirm("Queue maintenance", "The service will run its configured maintenance policy. This may change lifecycle states and consolidate duplicates.".into(), Change::RunMaintenance { project }, false, window, cx);
                        }
                    })),
            ),
        );
        content = content.child(maintenance);
        let checks = self.operations["doctor"]["checks"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        content = content.child(
            card(cx)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            Icon::new(IconName::ShieldCheck)
                                .size_4()
                                .text_color(theme.muted_foreground),
                        )
                        .child(section("DIAGNOSTICS", cx)),
                )
                .when(checks.is_empty(), |c| {
                    c.child(caption("No diagnostics reported.", cx))
                })
                .children(checks.iter().map(|check| {
                    let severity = string(check, "severity");
                    let tone = Tone::for_severity(&severity);
                    div()
                        .flex()
                        .items_start()
                        .gap_3()
                        .child(
                            div()
                                .pt(px(2.))
                                .child(Icon::new(tone.icon()).size_4().text_color(tone.color(cx))),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap_0p5()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(string(check, "id")),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(theme.muted_foreground)
                                        .child(string(check, "summary")),
                                ),
                        )
                })),
        );
        let jobs = self.operations["maintenance"]["recentJobs"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        content = content.child(
            card(cx)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            Icon::new(IconName::Activity)
                                .size_4()
                                .text_color(theme.muted_foreground),
                        )
                        .child(section("RECENT JOBS", cx)),
                )
                .when(jobs.is_empty(), |c| c.child(caption("No recent jobs.", cx)))
                .children(jobs.iter().map(|job| {
                    let state = string(job, "state");
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .min_w_0()
                        .child(chip(state.clone(), Tone::for_job(&state)))
                        .child(div().text_sm().child(string(job, "kind")))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .font_family(theme.mono_font_family.clone())
                                .truncate()
                                .child(string(job, "jobId")),
                        )
                })),
        );
        content.into_any_element()
    }

    fn service_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let version = string(&self.health, "daemonVersion");
        div()
            .id("service-panel")
            .size_full()
            .overflow_y_scroll()
            .p_5()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .p_4()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.border)
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(div().flex().items_center().gap_2().child(Icon::new(IconName::Monitor).size_4().text_color(theme.muted_foreground)).child(section("SHARED SERVICE", cx)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .text_sm()
                            .child(div().w(px(112.)).flex_none().text_color(theme.muted_foreground).child("State"))
                            .child(chip(if self.connected { "Connected" } else { "Unavailable" }, if self.connected { Tone::Success } else { Tone::Danger })),
                    )
                    .child(field("Version", if version.is_empty() { "—".to_string() } else { version }, cx))
                    .child(self.path_field("data-root", "Data root", self.root.path().display().to_string(), cx))
                    .child(field("Platform", format!("{} · {}", std::env::consts::OS, std::env::consts::ARCH), cx)),
            )
            .child(empty_state(
                IconName::ShieldCheck,
                "Service ownership",
                "This window is a client of the one per-user daemon. Closing it never stops WrongStack or other MCP clients.",
                cx,
            ))
            .into_any_element()
    }

    fn clients(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let project = self.project.clone().unwrap_or_default();
        let grants = self.grants.clone();
        div()
            .id("clients-content")
            .size_full()
            .overflow_y_scroll()
            .p_5()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().flex().items_center().gap_2().child(Icon::new(IconName::PlugZap).size_4().text_color(theme.muted_foreground)).child(section("MCP CLIENT ACCESS", cx)))
                    .child(div().text_sm().text_color(theme.muted_foreground).line_height(relative(1.5)).child("Each connection is bound to a project and a daemon-enforced grant. Credentials never appear in client configuration."))
                    .child(
                        div().flex().pt_1().child(
                            Button::new("create-grant")
                                .primary()
                                .small()
                                .icon(IconName::Plus)
                                .label("Create read / propose access")
                                .disabled(!self.connected || self.project.is_none() || self.changing)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.confirm(
                                        "Create client access",
                                        "Creates a read-propose grant for the selected project. It cannot accept its own proposals or directly edit records.".into(),
                                        Change::CreateGrant { project: project.clone(), label: "Desktop MCP access".into() },
                                        false,
                                        window,
                                        cx,
                                    )
                                })),
                        ),
                    ),
            )
            .when(grants.is_empty(), |c| c.child(caption("No client grants for this service yet.", cx)))
            .children(grants.into_iter().enumerate().map(|(ix, g)| {
                let grant = string(&g, "grantId");
                let id = grant.clone();
                let project = self.project.clone().unwrap_or_default();
                let revoked = !g["revokedAtMs"].is_null();
                let available = !revoked && g["projects"].as_array().is_some_and(|p| p.iter().any(|p| p.as_str() == Some(&project)));
                div()
                    .p_4()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.border)
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(Icon::new(IconName::KeyRound).size_4().text_color(theme.muted_foreground))
                            .child(div().flex_1().min_w_0().text_sm().font_weight(FontWeight::MEDIUM).truncate().child(string(&g, "label")))
                            .child(chip(string(&g, "profile"), Tone::Info))
                            .child(chip(if revoked { "Revoked" } else { "Active" }, if revoked { Tone::Danger } else { Tone::Success })),
                    )
                    .child(div().text_xs().font_family(theme.mono_font_family.clone()).text_color(theme.muted_foreground).child(grant.clone()))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                Button::new(("copy-config", ix))
                                    .small()
                                    .icon(IconName::Copy)
                                    .label("Copy MCP configuration")
                                    .disabled(!available || self.cli.is_none())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(cli) = &this.cli {
                                            let entry = service::mcp_entry(cli, &this.root, &project, &id);
                                            cx.write_to_clipboard(ClipboardItem::new_string(serde_json::to_string_pretty(&entry).unwrap_or_default()));
                                            this.notice = "Secret-free JSON MCP configuration copied. Use the matching client configuration format.".into();
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child({
                                let id = grant.clone();
                                Button::new(("revoke", ix))
                                    .danger()
                                    .ghost()
                                    .small()
                                    .label("Revoke")
                                    .disabled(!self.connected || revoked || self.changing)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.confirm("Revoke client access", "The client's future reads and writes will be refused. Type CONFIRM to continue.".into(), Change::RevokeGrant { grant: id.clone() }, true, window, cx)
                                    }))
                            }),
                    )
            }))
            .into_any_element()
    }

    fn palette_overlay(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let row = |id: SharedString,
                   icon: IconName,
                   label: SharedString,
                   hint: Option<Kbd>,
                   theme: &Theme| {
            Button::new(id)
                .ghost()
                .w_full()
                .h_auto()
                .px_3()
                .py_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(Icon::new(icon).size_4().text_color(theme.muted_foreground))
                        .child(div().flex_1().text_sm().truncate().child(label))
                        .children(hint),
                )
        };
        let projects = self.projects.clone();
        div()
            .id("palette-backdrop")
            .absolute()
            .inset_0()
            .bg(gpui_kit::black().opacity(if theme.is_dark() { 0.55 } else { 0.28 }))
            .flex()
            .justify_center()
            .pt(px(96.))
            .on_click(cx.listener(|this, _, window, cx| this.set_palette(false, window, cx)))
            .child(
                div()
                    .id("palette")
                    .w(px(480.))
                    .max_h(px(520.))
                    .rounded_xl()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.popover)
                    .text_color(theme.popover_foreground)
                    .shadow_xl()
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .key_context("CommandPalette")
                    .on_action(|_: &FocusNextItem, window, cx| window.focus_next(cx))
                    .on_action(|_: &FocusPreviousItem, window, cx| window.focus_prev(cx))
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .px_4()
                            .py_3()
                            .border_b_1()
                            .border_color(theme.border)
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Icon::new(IconName::Command)
                                    .size_4()
                                    .text_color(theme.muted_foreground),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child("Command palette"),
                            )
                            .child(Kbd::new(
                                Keystroke::parse("escape").expect("static keystroke parses"),
                            )),
                    )
                    .child(
                        div()
                            .id("palette-items")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .p_2()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(div().px_3().pt_1().pb_1().child(section("GO TO", cx)))
                            .children(VIEWS.into_iter().map(|view| {
                                row(
                                    format!("jump-{}", view.title()).into(),
                                    view.icon(),
                                    view.title().into(),
                                    None,
                                    &theme,
                                )
                                .when(view == View::Library, |first| {
                                    first.track_focus(&self.palette_focus)
                                })
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.set_palette(false, window, cx);
                                        this.navigate(view, window, cx)
                                    },
                                ))
                            }))
                            .child(div().px_3().pt_2().pb_1().child(section("ACTIONS", cx)))
                            .child(
                                row(
                                    "cmd-search".into(),
                                    IconName::Search,
                                    "Search knowledge".into(),
                                    Some(kbd("f")),
                                    &theme,
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.set_palette(false, window, cx);
                                        if this.view != View::Library
                                            && this.view != View::Relations
                                        {
                                            this.navigate(View::Library, window, cx);
                                        }
                                        this.query.read(cx).focus_handle(cx).focus(window, cx);
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                row(
                                    "cmd-refresh".into(),
                                    IconName::RefreshCw,
                                    "Refresh".into(),
                                    Some(kbd("r")),
                                    &theme,
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.set_palette(false, window, cx);
                                        this.overview(window, cx);
                                        this.load(window, cx);
                                    },
                                )),
                            )
                            .child(
                                row(
                                    "cmd-theme".into(),
                                    if theme.is_dark() {
                                        IconName::Sun
                                    } else {
                                        IconName::Moon
                                    },
                                    if theme.is_dark() {
                                        "Use light appearance".into()
                                    } else {
                                        "Use dark appearance".into()
                                    },
                                    None,
                                    &theme,
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.set_palette(false, window, cx);
                                        this.toggle_theme(window, cx);
                                    },
                                )),
                            )
                            .when(!projects.is_empty(), |list| {
                                list.child(
                                    div().px_3().pt_2().pb_1().child(section("PROJECTS", cx)),
                                )
                                .children(
                                    projects.into_iter().map(|p| {
                                        let id = p.project_id.clone();
                                        row(
                                            format!("cmd-project-{id}").into(),
                                            IconName::Folder,
                                            p.display_name.clone().into(),
                                            None,
                                            &theme,
                                        )
                                        .on_click(
                                            cx.listener(move |this, _, window, cx| {
                                                this.set_palette(false, window, cx);
                                                this.select_project(id.clone(), window, cx);
                                            }),
                                        )
                                    }),
                                )
                            }),
                    ),
            )
            .into_any_element()
    }
}

impl Render for Workbench {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self.focus.clone();
        let theme = cx.theme().clone();
        let sidebar = self.sidebar(cx);
        let toolbar = self.toolbar(cx);
        let (body, inspector) = match self.view {
            View::Library => (self.library(window, cx), self.inspector(cx)),
            View::Review => (self.review(cx), self.comparison(cx)),
            View::Relations => (self.relations(cx), self.inspector(cx)),
            View::Clients => (
                self.clients(cx),
                self.empty(IconName::PlugZap, "Connection policy", "Read / propose is the default. Client discovery does not prove automatic recall; validate the actual client and version.", cx),
            ),
            View::Health => (self.health_view(cx), self.service_panel(cx)),
        };
        let mut main = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .child(toolbar);
        // While disconnected the service banner below carries the transport
        // error; repeating it as a second banner adds noise, not information.
        if let Some(error) = self
            .error
            .as_ref()
            .filter(|e| self.connected || e.code != "UNAVAILABLE")
        {
            main = main.child(banner(
                if error.uncertain {
                    Tone::Warning
                } else {
                    Tone::Danger
                },
                format!(
                    "{}{}",
                    error.code,
                    if error.uncertain {
                        " · outcome unknown"
                    } else {
                        ""
                    }
                ),
                error.message.clone(),
                cx,
            ));
        }
        if !self.connected {
            main = main.child(
                banner(
                    Tone::Warning,
                    "Service unavailable",
                    if self.cli.is_some() {
                        "Existing knowledge stays intact. Start the shared per-user service to continue; WrongStack and MCP clients use the same daemon."
                    } else {
                        "Existing knowledge stays intact. Install the WrongMemory CLI or pass --cli to enable Start service."
                    },
                    cx,
                )
                .child(
                    Button::new("start-service")
                        .primary()
                        .small()
                        .icon(IconName::Play)
                        .label("Start service")
                        .loading(self.starting)
                        .disabled(self.cli.is_none() || self.starting)
                        .on_click(cx.listener(|this, _, window, cx| this.start(window, cx))),
                ),
            );
        }
        if self.project.is_none() && !self.connected {
            main = main.child(self.empty(IconName::Monitor, "Waiting for the shared service", "Projects and knowledge load once the per-user WrongMemory service is reachable. Nothing is read from disk directly.", cx));
        } else if self.project.is_none() && self.connected {
            main = main.child(self.empty(IconName::FolderOpen, "No projects registered", "Register a managed project with `wrongmemory project add <path> --managed`. The desktop discovers it after Refresh.", cx));
        } else {
            main = main.child(
                div().flex_1().min_h_0().child(
                    h_resizable("working-panels")
                        .on_resize(cx.listener(|this, state: &Entity<ResizableState>, _, cx| {
                            if let Some(width) = state.read(cx).sizes().first() {
                                this.preferences.library_width =
                                    f32::from(*width).clamp(340., 1100.);
                                this.persist(cx);
                            }
                        }))
                        .child(
                            resizable_panel()
                                .size(px(self.preferences.library_width))
                                .size_range(px(340.)..px(1100.))
                                .child(body),
                        )
                        .child(
                            resizable_panel().size_range(px(300.)..px(1300.)).child(
                                div()
                                    .size_full()
                                    .border_l_1()
                                    .border_color(theme.border)
                                    .child(inspector),
                            ),
                        ),
                ),
            );
        }
        let status_text: SharedString = if !self.connected {
            "Disconnected · showing the last loaded view".into()
        } else if self.changing {
            "Saving…".into()
        } else if self.notice.is_empty() {
            format!("Connected · v{}", string(&self.health, "daemonVersion")).into()
        } else {
            self.notice.clone().into()
        };
        let mut shell = div()
            .id("wrongmemory-workbench")
            .key_context("WrongMemoryDesktop")
            .track_focus(&focus)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_color(theme.foreground)
            .font_family(theme.font_family.clone())
            .on_action(cx.listener(|this, _: &Refresh, window, cx| {
                this.overview(window, cx);
                this.load(window, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                this.query.read(cx).focus_handle(cx).focus(window, cx)
            }))
            .on_action(cx.listener(|this, _: &TogglePalette, window, cx| {
                this.set_palette(!this.palette, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &NextRecord, window, cx| this.select_offset(1, window, cx)),
            )
            .on_action(cx.listener(|this, _: &PreviousRecord, window, cx| {
                this.select_offset(-1, window, cx)
            }))
            .on_action(cx.listener(|this, _: &Escape, window, cx| {
                if this.palette {
                    this.set_palette(false, window, cx)
                }
            }))
            .child(
                div().flex_1().min_h_0().child(
                    h_resizable("application-panels")
                        .on_resize(cx.listener(|this, state: &Entity<ResizableState>, _, cx| {
                            if let Some(width) = state.read(cx).sizes().first() {
                                this.preferences.sidebar_width =
                                    f32::from(*width).clamp(220., 380.);
                                this.persist(cx);
                            }
                        }))
                        .child(
                            resizable_panel()
                                .size(px(self.preferences.sidebar_width))
                                .size_range(px(220.)..px(380.))
                                .child(sidebar),
                        )
                        .child(resizable_panel().child(main)),
                ),
            )
            .child(
                div()
                    .h(px(28.))
                    .flex_none()
                    .px_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .bg(theme.status_bar)
                    .border_t_1()
                    .border_color(theme.status_bar_border)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .min_w_0()
                            .child(div().flex_none().size(px(7.)).rounded_full().bg(
                                if self.connected {
                                    theme.success
                                } else {
                                    theme.danger
                                },
                            ))
                            .child(div().truncate().child(status_text)),
                    )
                    .child(if self.editing {
                        div()
                            .flex_none()
                            .child("Draft protected · revision checked on save")
                            .into_any_element()
                    } else {
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(kbd("f"))
                                    .child("Search"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(kbd("k"))
                                    .child("Commands"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(kbd("r"))
                                    .child("Refresh"),
                            )
                            .into_any_element()
                    }),
            );
        if self.palette {
            shell = shell.child(self.palette_overlay(cx));
        }
        shell
    }
}
