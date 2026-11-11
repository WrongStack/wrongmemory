mod graph_view;

use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
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

gpui_kit::actions!(
    desktop,
    [
        Refresh,
        FocusSearch,
        TogglePalette,
        NextRecord,
        PreviousRecord,
        Escape
    ]
);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-r", Refresh, Some("WrongMemoryDesktop")),
        KeyBinding::new("ctrl-f", FocusSearch, Some("WrongMemoryDesktop")),
        KeyBinding::new("ctrl-k", TogglePalette, Some("WrongMemoryDesktop")),
        KeyBinding::new("ctrl-p", TogglePalette, Some("WrongMemoryDesktop")),
        KeyBinding::new("down", NextRecord, Some("MemoryLibrary")),
        KeyBinding::new("up", PreviousRecord, Some("MemoryLibrary")),
        KeyBinding::new("escape", Escape, Some("WrongMemoryDesktop")),
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
        let owner = cx.entity().downgrade();
        this._subscriptions
            .push(window.observe_window_appearance(move |window, cx| {
                let _ = owner.update(cx, |this, cx| {
                    if this.preferences.appearance == Appearance::System {
                        Theme::change(ThemeMode::from(window.appearance()), Some(window), cx);
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
                    Ok(()) => this.overview(window, cx),
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

fn section(label: impl Into<SharedString>) -> Div {
    div()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .child(label.into())
}
fn caption(label: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(label.into())
}
fn badge(label: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .px_2()
        .py_1()
        .rounded_md()
        .bg(cx.theme().muted)
        .text_xs()
        .child(label.into())
}

impl Workbench {
    fn sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let filter = self.project_filter.read(cx).value().to_lowercase();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().sidebar)
            .p_4()
            .gap_4()
            .child(
                div()
                    .flex()
                    .gap_2()
                    .items_center()
                    .child(Icon::new(IconName::Database).text_color(cx.theme().primary))
                    .child(div().font_weight(FontWeight::BOLD).child("WrongMemory")),
            )
            .child(caption("PROJECT KNOWLEDGE", cx))
            .child(Input::new(&self.project_filter))
            .child(
                div()
                    .id("projects")
                    .flex_1()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(
                        self.projects
                            .iter()
                            .filter(|p| {
                                p.display_name.to_lowercase().contains(&filter)
                                    || p.root.to_lowercase().contains(&filter)
                            })
                            .map(|p| {
                                let id = p.project_id.clone();
                                let active = self.project.as_ref() == Some(&id);
                                Button::new(SharedString::from(format!("project-{id}")))
                                    .ghost()
                                    .label(p.display_name.clone())
                                    .selected(active)
                                    .w_full()
                                    .justify_start()
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.select_project(id.clone(), window, cx)
                                    }))
                            }),
                    ),
            )
            .child(caption(
                format!("{} registered projects", self.projects.len()),
                cx,
            ))
            .child(
                div()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .pt_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        Button::new("palette")
                            .ghost()
                            .label("Command palette   Ctrl K")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.palette = !this.palette;
                                cx.notify();
                            })),
                    )
                    .child(caption("One service · all your coding tools", cx)),
            )
            .into_any_element()
    }

    fn toolbar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let name = self
            .current_project()
            .map(|p| p.display_name.clone())
            .unwrap_or_else(|| "No project selected".into());
        div()
            .flex()
            .flex_col()
            .gap_4()
            .p_5()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(caption(name, cx))
                            .child(
                                div()
                                    .text_2xl()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(self.view.title()),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("refresh")
                                    .ghost()
                                    .icon(IconName::RefreshCw)
                                    .label("Refresh")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.overview(window, cx);
                                        this.load(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("theme")
                                    .ghost()
                                    .label(if cx.theme().is_dark() {
                                        "Light"
                                    } else {
                                        "Dark"
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let mode = if cx.theme().is_dark() {
                                            ThemeMode::Light
                                        } else {
                                            ThemeMode::Dark
                                        };
                                        this.preferences.appearance = if mode == ThemeMode::Light {
                                            Appearance::Light
                                        } else {
                                            Appearance::Dark
                                        };
                                        Theme::change(mode, Some(window), cx);
                                        this.persist(cx);
                                    })),
                            ),
                    ),
            )
            .child(
                div().flex().gap_1().children(
                    [
                        View::Library,
                        View::Review,
                        View::Relations,
                        View::Clients,
                        View::Health,
                    ]
                    .into_iter()
                    .map(|view| {
                        Button::new(view.title())
                            .ghost()
                            .icon(view.icon())
                            .label(view.title())
                            .selected(self.view == view)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.navigate(view, window, cx)
                            }))
                    }),
                ),
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
        let count = self.page.memories.len();
        div().size_full().flex().flex_col().key_context("MemoryLibrary").track_focus(&self.list_focus)
            .child(div().p_4().flex().flex_col().gap_3().child(Input::new(&self.query).disabled(self.editing))
                .child(div().flex().flex_wrap().gap_1().children(statuses.into_iter().map(|(label,status)| {
                    Button::new(label).ghost().small().label(label).selected(self.status.as_deref()==status).disabled(self.editing).on_click(cx.listener(move |this,_,window,cx| {this.status=status.map(str::to_string);this.persist(cx);this.cursor=None;this.previous_cursors.clear();this.load(window,cx);} ))
                })))
                .child(caption(if self.loading {"Loading project knowledge…".into()}else{format!("{} records · {} on this page",self.page.total,count)},cx)))
            .child(if count==0 {self.empty("No matching memories","Try another query or status. Records are never fabricated for an empty project.",cx)} else {
                uniform_list("memory-list",count,cx.processor(|this,range: std::ops::Range<usize>,_,cx| {
                    range.map(|ix| {
                        let memory=this.page.memories[ix].clone(); let selected=this.selected.as_ref().is_some_and(|m|m.id==memory.id);
                        div().id(("memory",ix)).role(Role::Button).aria_label(format!("{} memory, revision {}: {}",memory.kind,memory.revision,preview(&memory.text,160))).h(px(116.)).w_full().px_4().py_2().cursor_pointer()
                            .bg(if selected{cx.theme().accent}else{cx.theme().background})
                            .border_b_1().border_color(cx.theme().border).hover(|s|s.bg(cx.theme().accent))
                            .on_click(cx.listener(move |this,_,window,cx| {this.list_focus.focus(window,cx);this.inspect(memory.clone(),window,cx);} ))
                            .child(div().flex().flex_col().gap_2().overflow_hidden()
                                .child(div().text_xs().text_color(cx.theme().muted_foreground).child(format!("{}   ·   r{}",this.page.memories[ix].kind,this.page.memories[ix].revision)))
                                .child(div().text_sm().child(preview(&this.page.memories[ix].text,135)))
                                .child(caption(this.page.memories[ix].anchors.first().map(|a|string(a,"path")).unwrap_or_default(),cx)))
                    }).collect::<Vec<_>>()
                })).track_scroll(&self.list_scroll).flex_1().into_any_element()
            })
            .child(div().p_3().border_t_1().border_color(cx.theme().border).flex().justify_between()
                .child(Button::new("previous-page").ghost().small().label("Previous").disabled(self.previous_cursors.is_empty()||self.loading||self.editing).on_click(cx.listener(|this,_,window,cx|{this.cursor=this.previous_cursors.pop().flatten();this.load(window,cx);})))
                .child(Button::new("next-page").ghost().small().label("Next page").disabled(self.page.next_cursor.is_none()||self.loading||self.editing).on_click(cx.listener(|this,_,window,cx|{this.previous_cursors.push(this.cursor.clone());this.cursor=this.page.next_cursor.clone();this.load(window,cx);}))))
            .into_any_element()
    }

    fn empty(&self, title: &str, description: &str, cx: &App) -> AnyElement {
        div()
            .flex_1()
            .p_8()
            .flex()
            .flex_col()
            .justify_center()
            .gap_3()
            .child(
                Icon::new(IconName::BookOpen)
                    .size_6()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title.to_string()),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(description.to_string()),
            )
            .into_any_element()
    }

    fn inspector(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(memory) = self.selected.clone() else {
            return self.empty(
                "Evidence inspector",
                "Select a memory to inspect its full text, source anchors and current revision.",
                cx,
            );
        };
        let project = self.project.clone().unwrap_or_default();
        let mut toolbar = div().flex().flex_wrap().gap_2();
        if self.editing {
            toolbar = toolbar
                .child(
                    Button::new("save-draft")
                        .primary()
                        .label("Save revision")
                        .disabled(!self.writable())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.save_draft(window, cx);
                        })),
                )
                .child(
                    Button::new("cancel-draft")
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
                        .label("Compare current revision")
                        .disabled(self.changing)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.fetch_draft_base(window, cx)),
                        ),
                );
            if let Some(server) = self.draft.as_ref().and_then(|d| d.server.as_ref()) {
                toolbar=toolbar.child(Button::new("rebase-draft").label(format!("Use revision {} for next save",server.revision)).disabled(self.changing).on_click(cx.listener(|this,_,_,cx|{
                    if let Some(draft)=this.draft.as_mut(){match draft.rebase(){Ok(revision)=>{this.selected=Some(draft.base.clone());this.error=None;this.notice=format!("Draft retained. The next save is guarded by revision {revision}.");},Err(message)=>this.notice=message.into()}}
                    cx.notify();
                })));
            }
        } else {
            toolbar = toolbar
                .child(
                    Button::new("edit-memory")
                        .primary()
                        .label("Edit memory")
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
                toolbar=toolbar.child(Button::new("recover-memory").label("Recover").disabled(!self.writable()).on_click(cx.listener(move|this,_,window,cx|this.confirm("Recover memory", "The service will validate the current lifecycle and successor chain.".into(),Change::Recover{project:pid.clone(),id:id.clone()},false,window,cx))));
            }
            if memory.status != "deleted" {
                let id = memory.id.clone();
                let pid = project.clone();
                toolbar=toolbar.child(Button::new("delete-memory").danger().label("Delete").disabled(!self.writable()).on_click(cx.listener(move|this,_,window,cx|this.confirm("Delete memory", "Creates a recoverable tombstone. This also authorizes the force override for a permanent record. Type CONFIRM to continue.".into(),Change::Delete{project:pid.clone(),id:id.clone(),reason:"Deleted by the native desktop operator".into()},true,window,cx))));
            }
        }
        let anchors = memory
            .anchors
            .iter()
            .map(|a| {
                format!(
                    "{}  {}{}",
                    string(a, "type"),
                    string(a, "path"),
                    if string(a, "symbol").is_empty() {
                        String::new()
                    } else {
                        format!(" #{}", string(a, "symbol"))
                    }
                )
            })
            .collect::<Vec<_>>();
        let sources = memory
            .sources
            .iter()
            .map(|s| serde_json::to_string_pretty(s).unwrap_or_default())
            .collect::<Vec<_>>();
        div().size_full().flex().flex_col().p_5().gap_4()
            .child(div().flex().justify_between().items_center().child(section(if self.editing{"EDIT DRAFT"}else{"EVIDENCE INSPECTOR"})).child(badge(format!("Revision {}",memory.revision),cx)))
            .child(div().flex().flex_wrap().gap_2().child(badge(memory.kind.clone(),cx)).child(badge(memory.status.clone(),cx)).children(memory.tags.iter().take(8).map(|tag|badge(format!("#{tag}"),cx))))
            .child(caption(memory.id.clone(),cx))
            .child(toolbar)
            .child(div().flex_1().min_h(px(200.)).child(Textarea::new(&self.editor).readonly(!self.editing).size_full()))
            .when(self.editing&&self.draft.as_ref().is_some_and(|d|d.server.is_some()),|pane|pane.child(div().flex().flex_col().gap_2().child(section("CURRENT SERVER TEXT · DRAFT ABOVE IS UNCHANGED")).child(div().h(px(150.)).child(Textarea::new(&self.target_editor).readonly(true).size_full()))))
            .child(div().id("evidence-fields").max_h(px(240.)).overflow_y_scroll().flex().flex_col().gap_3()
                .child(section("ANCHORS"))
                .children(if anchors.is_empty(){vec![caption("No structural anchors",cx)]}else{anchors.into_iter().map(|a|caption(a,cx)).collect()})
                .child(section("PROVENANCE"))
                .children(if sources.is_empty(){vec![caption("No recorded source",cx)]}else{sources.into_iter().map(|s|caption(s,cx)).collect()})
                .child(section("APPLICABILITY"))
                .child(caption(memory.extra.get("validity").map(|v|v.to_string()).unwrap_or_else(||"No explicit applicability checks. Source presence is not a truth guarantee.".into()),cx)))
            .into_any_element()
    }

    fn review(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.candidates.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .p_4()
            .gap_3()
            .child(caption(
                format!("{} pending proposals · select to compare", rows.len()),
                cx,
            ))
            .child(
                div()
                    .id("review-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(rows.into_iter().enumerate().map(|(ix, c)| {
                        let selected = self
                            .selected_candidate
                            .as_ref()
                            .is_some_and(|s| s["id"] == c["id"]);
                        let item = c.clone();
                        div()
                            .id(("proposal", ix))
                            .p_4()
                            .rounded_md()
                            .bg(if selected {
                                cx.theme().accent
                            } else {
                                cx.theme().muted
                            })
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.select_candidate(item.clone(), window, cx)
                            }))
                            .child(caption(string(&c, "kind"), cx))
                            .child(
                                div()
                                    .mt_2()
                                    .text_sm()
                                    .child(preview(&string(&c, "text"), 240)),
                            )
                            .child(caption(
                                if string(&c, "targetMemoryId").is_empty() {
                                    "New knowledge".into()
                                } else {
                                    format!(
                                        "Revises {} · r{}",
                                        string(&c, "targetMemoryId"),
                                        c["targetRevision"]
                                    )
                                },
                                cx,
                            ))
                    })),
            )
            .into_any_element()
    }

    fn comparison(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(candidate) = self.selected_candidate.clone() else {
            return self.empty("Review with evidence","Select a proposal. Its evidence and the current target are shown before any decision.",cx);
        };
        let id = string(&candidate, "id");
        let project = self.project.clone().unwrap_or_default();
        let conflict = self.selected.as_ref().is_some_and(|m| {
            candidate["targetRevision"]
                .as_i64()
                .is_some_and(|r| r != m.revision)
        });
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
                actions=actions.child(Button::new(label).label(label).disabled(!can_decide).on_click(cx.listener(move|this,_,window,cx|this.confirm("Resolve review",format!("Apply {decision} to the revision-bound target. The service revalidates it before writing."),Change::Resolve{project:pid.clone(),candidate:id.clone(),decision:decision.into()},decision=="delete",window,cx))));
            }
        } else {
            let id = id.clone();
            let pid = project.clone();
            actions=actions.child(Button::new("accept-proposal").primary().label("Accept proposal").disabled(!can_decide).on_click(cx.listener(move|this,_,window,cx|this.confirm("Accept proposal","Commit this proposal as authoritative project knowledge. The service checks the target revision.".into(),Change::Accept{project:pid.clone(),candidate:id.clone()},false,window,cx))));
        }
        actions = actions.child(
            Button::new("reject-proposal")
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
        div()
            .size_full()
            .p_5()
            .flex()
            .flex_col()
            .gap_3()
            .child(section("REVIEW WORKBENCH"))
            .child(caption(
                if conflict {
                    "Revision conflict — refresh the proposal before deciding."
                } else {
                    "Compare the current target, proposed knowledge and evidence."
                },
                cx,
            ))
            .child(actions)
            .child(section("CURRENT TARGET"))
            .child(
                div().h(px(180.)).child(
                    Textarea::new(&self.target_editor)
                        .readonly(true)
                        .size_full(),
                ),
            )
            .child(section("PROPOSED KNOWLEDGE"))
            .child(
                div()
                    .flex_1()
                    .min_h(px(180.))
                    .child(Textarea::new(&self.editor).readonly(true).size_full()),
            )
            .child(caption(
                candidate
                    .get("evidence")
                    .or_else(|| candidate.get("reviewReason"))
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| {
                        "Inspect the proposal's recorded sources before accepting.".into()
                    }),
                cx,
            ))
            .into_any_element()
    }

    fn health_view(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let mut content = div()
            .id("health-content")
            .size_full()
            .overflow_y_scroll()
            .p_6()
            .flex()
            .flex_col()
            .gap_5();
        let stats = &self.operations["stats"];
        let schedule = &self.operations["maintenance"]["schedule"];
        content=content.child(section("PROJECT STORAGE"))
            .child(caption(self.current_project().map(|p|format!("{} · {}",p.storage_profile_id.as_deref().unwrap_or("Unknown profile"),p.admission_reason)).unwrap_or_default(),cx))
            .child(caption(self.current_project().map(|p|p.store_path.clone()).unwrap_or_default(),cx))
            .child(div().flex().flex_wrap().gap_3().children(stats["byStatus"].as_object().into_iter().flatten().map(|(status,n)|badge(format!("{status}  {n}"),cx))))
            .child(section("MAINTENANCE"))
            .child(caption(if schedule.is_null(){"No recorded schedule".into()}else{format!("Schedule: {}",schedule)},cx))
            .child(Button::new("run-maintenance").label("Queue maintenance").disabled(!self.writable()).on_click(cx.listener(|this,_,window,cx|{if let Some(project)=this.project.clone(){this.confirm("Queue maintenance","The service will run its configured maintenance policy. This may change lifecycle states and consolidate duplicates.".into(),Change::RunMaintenance{project},false,window,cx);} })))
            .child(section("DIAGNOSTICS"));
        for check in self.operations["doctor"]["checks"]
            .as_array()
            .into_iter()
            .flatten()
        {
            content = content.child(
                div()
                    .p_3()
                    .rounded_md()
                    .bg(cx.theme().muted)
                    .child(section(format!(
                        "{} · {}",
                        string(check, "severity"),
                        string(check, "id")
                    )))
                    .child(div().mt_2().text_sm().child(string(check, "summary"))),
            );
        }
        content = content.child(section("RECENT JOBS"));
        for job in self.operations["maintenance"]["recentJobs"]
            .as_array()
            .into_iter()
            .flatten()
        {
            content = content.child(caption(
                format!(
                    "{} · {} · {}",
                    string(job, "state"),
                    string(job, "kind"),
                    string(job, "jobId")
                ),
                cx,
            ));
        }
        content.into_any_element()
    }

    fn clients(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let project = self.project.clone().unwrap_or_default();
        let grants = self.grants.clone();
        div().id("clients-content").size_full().overflow_y_scroll().p_6().flex().flex_col().gap_4()
            .child(section("MCP CLIENT ACCESS"))
            .child(caption("Each connection is bound to a project and a daemon-enforced grant. Credentials never appear in client configuration.",cx))
            .child(Button::new("create-grant").primary().label("Create read / propose access").disabled(!self.connected||self.project.is_none()||self.changing).on_click(cx.listener(move|this,_,window,cx|this.confirm("Create client access", "Creates a read-propose grant for the selected project. It cannot accept its own proposals or directly edit records.".into(),Change::CreateGrant{project:project.clone(),label:"Desktop MCP access".into()},false,window,cx))))
            .children(grants.into_iter().enumerate().map(|(ix,g)| {
                let grant=string(&g,"grantId");let id=grant.clone();let project=self.project.clone().unwrap_or_default();
                let available=g["revokedAtMs"].is_null() && g["projects"].as_array().is_some_and(|p|p.iter().any(|p|p.as_str()==Some(&project)));
                let mut row=div().p_4().rounded_md().border_1().border_color(cx.theme().border).flex().flex_col().gap_2()
                    .child(section(string(&g,"label"))).child(caption(format!("{} · {} · {}",string(&g,"profile"),grant,if g["revokedAtMs"].is_null(){"Active / check expiry"}else{"Revoked"}),cx));
                let mut actions=div().flex().gap_2();
                actions=actions.child(Button::new(("copy-config",ix)).label("Copy MCP configuration").disabled(!available||self.cli.is_none()).on_click(cx.listener(move|this,_,_,cx|{
                    if let Some(cli)=&this.cli {let entry=service::mcp_entry(cli,&this.root,&project,&id);cx.write_to_clipboard(ClipboardItem::new_string(serde_json::to_string_pretty(&entry).unwrap_or_default()));this.notice="Secret-free JSON MCP configuration copied. Use the matching client configuration format.".into();cx.notify();}
                })));
                let id=grant.clone();
                actions=actions.child(Button::new(("revoke",ix)).danger().label("Revoke").disabled(!self.connected||!g["revokedAtMs"].is_null()||self.changing).on_click(cx.listener(move|this,_,window,cx|this.confirm("Revoke client access", "The client's future reads and writes will be refused. Type CONFIRM to continue.".into(),Change::RevokeGrant{grant:id.clone()},true,window,cx))));
                row=row.child(actions);row
            }))
            .into_any_element()
    }
}

impl Render for Workbench {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self.focus.clone();
        let sidebar = self.sidebar(cx);
        let toolbar = self.toolbar(cx);
        let (body,inspector)=match self.view {
            View::Library=>(self.library(window,cx),self.inspector(cx)),
            View::Review=>(self.review(cx),self.comparison(cx)),
            View::Relations=>(self.relations(cx),self.inspector(cx)),
            View::Clients=>(self.clients(cx),self.empty("Connection policy","Read / propose is the default. Client discovery does not prove automatic recall; validate the actual client and version.",cx)),
            View::Health=>(self.health_view(cx),self.empty("Service ownership","This window is a client of the existing daemon. Closing it never stops WrongStack or other MCP clients.",cx)),
        };
        let mut main = div().size_full().flex().flex_col().child(toolbar);
        if let Some(error) = &self.error {
            main = main.child(
                div()
                    .p_4()
                    .bg(cx.theme().muted)
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(section(format!(
                        "{}{}",
                        error.code,
                        if error.uncertain {
                            " · OUTCOME UNKNOWN"
                        } else {
                            ""
                        }
                    )))
                    .child(div().text_sm().child(error.message.clone())),
            );
        }
        if !self.connected {
            main = main.child(
                div()
                    .p_5()
                    .flex()
                    .gap_3()
                    .items_center()
                    .child(caption(
                        "Service unavailable. Existing knowledge stays intact.",
                        cx,
                    ))
                    .child(
                        Button::new("start-service")
                            .primary()
                            .label("Start service")
                            .disabled(self.cli.is_none() || self.starting)
                            .on_click(cx.listener(|this, _, window, cx| this.start(window, cx))),
                    ),
            );
        }
        if self.project.is_none() && self.connected {
            main=main.child(self.empty("No projects registered","Register a managed project with wrongmemory project add <path> --managed. The desktop will discover it after Refresh.",cx));
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
                            resizable_panel()
                                .size_range(px(300.)..px(1300.))
                                .child(inspector),
                        ),
                ),
            );
        }
        let mut shell = div()
            .id("wrongmemory-workbench")
            .key_context("WrongMemoryDesktop")
            .track_focus(&focus)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .font_family(cx.theme().font_family.clone())
            .on_action(cx.listener(|this, _: &Refresh, window, cx| {
                this.overview(window, cx);
                this.load(window, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                this.query.read(cx).focus_handle(cx).focus(window, cx)
            }))
            .on_action(cx.listener(|this, _: &TogglePalette, _, cx| {
                this.palette = !this.palette;
                cx.notify();
            }))
            .on_action(
                cx.listener(|this, _: &NextRecord, window, cx| this.select_offset(1, window, cx)),
            )
            .on_action(cx.listener(|this, _: &PreviousRecord, window, cx| {
                this.select_offset(-1, window, cx)
            }))
            .on_action(cx.listener(|this, _: &Escape, _, cx| {
                this.palette = false;
                cx.notify();
            }))
            .child(
                div().flex_1().min_h_0().child(
                    h_resizable("application-panels")
                        .on_resize(cx.listener(|this, state: &Entity<ResizableState>, _, cx| {
                            if let Some(width) = state.read(cx).sizes().first() {
                                this.preferences.sidebar_width =
                                    f32::from(*width).clamp(195., 380.);
                                this.persist(cx);
                            }
                        }))
                        .child(
                            resizable_panel()
                                .size(px(self.preferences.sidebar_width))
                                .size_range(px(195.)..px(380.))
                                .child(sidebar),
                        )
                        .child(resizable_panel().child(main)),
                ),
            )
            .child(
                div()
                    .h_8()
                    .px_4()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(caption(
                        if !self.connected {
                            "Disconnected · showing the last loaded view".into()
                        } else if self.changing {
                            "Saving…".into()
                        } else if self.notice.is_empty() {
                            if self.connected {
                                format!("Connected · v{}", string(&self.health, "daemonVersion"))
                            } else {
                                "Disconnected".into()
                            }
                        } else {
                            self.notice.clone()
                        },
                        cx,
                    ))
                    .child(caption(
                        if self.editing {
                            "Draft protected · revision checked on save"
                        } else {
                            "Ctrl F  Search     Ctrl K  Commands     Ctrl R  Refresh"
                        },
                        cx,
                    )),
            );
        if self.palette {
            shell = shell.child(
                div()
                    .absolute()
                    .top(px(76.))
                    .left(px(260.))
                    .w(px(440.))
                    .p_4()
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().popover)
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(section("JUMP TO"))
                    .children(
                        [
                            View::Library,
                            View::Review,
                            View::Relations,
                            View::Clients,
                            View::Health,
                        ]
                        .into_iter()
                        .map(|view| {
                            Button::new(("jump", view as usize))
                                .ghost()
                                .label(view.title())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.navigate(view, window, cx)
                                }))
                        }),
                    )
                    .child(
                        Button::new("close-palette")
                            .ghost()
                            .label("Close · Escape")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.palette = false;
                                cx.notify();
                            })),
                    ),
            );
        }
        shell
    }
}
