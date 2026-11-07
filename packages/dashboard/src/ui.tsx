import {
  BookOpen,
  Brain,
  Bug,
  CircleAlert,
  CircleCheck,
  CircleX,
  Ban,
  FileText,
  History,
  Info,
  Lightbulb,
  ListTodo,
  LoaderCircle,
  Ruler,
  Scale,
  ShieldAlert,
  Star,
  TriangleAlert,
  Workflow,
  Wrench,
  X,
} from 'lucide-react';
import {
  type ButtonHTMLAttributes,
  createContext,
  type ReactNode,
  useCallback,
  useContext,
  useEffect,
  useRef,
  useState,
} from 'react';
import { errorText } from './api';
import { humanize } from './format';

export const cx = (...c: (string | false | null | undefined)[]) => c.filter(Boolean).join(' ');

// ─── Buttons and chips ──────────────────────────────────────────────────────

type Tone = 'default' | 'primary' | 'danger' | 'ghost' | 'ok';

export function Button({
  tone = 'default',
  size = 'md',
  busy,
  icon,
  className,
  children,
  disabled,
  ...rest
}: ButtonHTMLAttributes<HTMLButtonElement> & { tone?: Tone; size?: 'sm' | 'md'; busy?: boolean; icon?: ReactNode }) {
  const tones: Record<Tone, string> = {
    default: 'border-line bg-panel-2 text-fg hover:bg-raised hover:border-line-strong',
    primary: 'border-accent bg-accent text-on-accent hover:brightness-110',
    danger: 'border-bad/60 bg-bad-soft text-bad hover:bg-bad hover:text-white',
    ok: 'border-ok/60 bg-ok-soft text-ok hover:bg-ok hover:text-white',
    ghost: 'border-transparent bg-transparent text-muted hover:bg-raised hover:text-fg',
  };
  return (
    <button
      type="button"
      disabled={disabled || busy}
      className={cx(
        'inline-flex shrink-0 items-center justify-center gap-1.5 rounded-md border font-medium transition-colors disabled:cursor-not-allowed disabled:opacity-50',
        size === 'sm' ? 'h-7 px-2 text-xs' : 'h-8 px-3 text-[13px]',
        tones[tone],
        className,
      )}
      {...rest}
    >
      {busy ? <LoaderCircle className="size-3.5 animate-spin" /> : icon}
      {children}
    </button>
  );
}

const STATUS_TONE: Record<string, string> = {
  active: 'bg-ok-soft text-ok border-ok/30',
  ok: 'bg-ok-soft text-ok border-ok/30',
  succeeded: 'bg-ok-soft text-ok border-ok/30',
  accepted: 'bg-ok-soft text-ok border-ok/30',
  'read-write': 'bg-ok-soft text-ok border-ok/30',
  stale: 'bg-warn-soft text-warn border-warn/30',
  warn: 'bg-warn-soft text-warn border-warn/30',
  superseded: 'bg-warn-soft text-warn border-warn/30',
  pending: 'bg-info-soft text-info border-info/30',
  queued: 'bg-info-soft text-info border-info/30',
  running: 'bg-info-soft text-info border-info/30',
  info: 'bg-info-soft text-info border-info/30',
  'read-only': 'bg-warn-soft text-warn border-warn/30',
  contradicted: 'bg-bad-soft text-bad border-bad/30',
  deleted: 'bg-bad-soft text-bad border-bad/30',
  error: 'bg-bad-soft text-bad border-bad/30',
  failed: 'bg-bad-soft text-bad border-bad/30',
  rejected: 'bg-bad-soft text-bad border-bad/30',
  revoked: 'bg-bad-soft text-bad border-bad/30',
};

export function Chip({ children, tone, className, title }: { children: ReactNode; tone?: string; className?: string; title?: string }) {
  return (
    <span
      title={title}
      className={cx(
        'inline-flex h-5 items-center gap-1 whitespace-nowrap rounded border px-1.5 text-[11px] font-medium',
        (tone && STATUS_TONE[tone]) || 'border-line bg-raised text-muted',
        className,
      )}
    >
      {children}
    </span>
  );
}

export const StatusChip = ({ status }: { status: string }) => <Chip tone={status}>{humanize(status)}</Chip>;

// ─── Memory kinds ───────────────────────────────────────────────────────────

const KIND_ICON: Record<string, typeof Brain> = {
  fact: BookOpen,
  decision: Scale,
  bug_root_cause: Bug,
  error_pattern: TriangleAlert,
  tool_outcome: Wrench,
  preference: Star,
  warning: ShieldAlert,
  convention: Ruler,
  task: ListTodo,
  file_note: FileText,
  symbol_note: FileText,
  command_note: FileText,
  insight: Lightbulb,
  workflow: Workflow,
  anti_pattern: Ban,
  security_signal: ShieldAlert,
  summary: BookOpen,
  session_digest: History,
  task_outcome: ListTodo,
  memory_review: Lightbulb,
};

const KIND_TONE: Record<string, string> = {
  anti_pattern: 'text-bad bg-bad-soft',
  security_signal: 'text-bad bg-bad-soft',
  workflow: 'text-ok bg-ok-soft',
  bug_root_cause: 'text-bad bg-bad-soft',
  error_pattern: 'text-bad bg-bad-soft',
  warning: 'text-warn bg-warn-soft',
  decision: 'text-accent bg-accent-soft',
  convention: 'text-accent bg-accent-soft',
  preference: 'text-warn bg-warn-soft',
  tool_outcome: 'text-ok bg-ok-soft',
  fact: 'text-info bg-info-soft',
  file_note: 'text-info bg-info-soft',
};

export function KindIcon({ kind, size = 'md' }: { kind: string; size?: 'sm' | 'md' }) {
  const Icon = KIND_ICON[kind] ?? Brain;
  return (
    <span
      title={humanize(kind)}
      className={cx(
        'inline-flex shrink-0 items-center justify-center rounded-md',
        size === 'sm' ? 'size-6' : 'size-8',
        KIND_TONE[kind] ?? 'bg-raised text-muted',
      )}
    >
      <Icon className={size === 'sm' ? 'size-3.5' : 'size-4'} />
    </span>
  );
}

export const kindLabel = humanize;

// ─── Layout pieces ──────────────────────────────────────────────────────────

export function Panel({ title, actions, children, className }: { title?: ReactNode; actions?: ReactNode; children: ReactNode; className?: string }) {
  return (
    <section className={cx('rounded-lg border border-line bg-panel', className)}>
      {(title || actions) && (
        <header className="flex items-center gap-2 border-b border-line px-4 py-2.5">
          <h3 className="text-[13px] font-semibold">{title}</h3>
          <div className="ml-auto flex items-center gap-2">{actions}</div>
        </header>
      )}
      <div className="p-4">{children}</div>
    </section>
  );
}

export function Stat({ label, value, hint, tone }: { label: string; value: ReactNode; hint?: ReactNode; tone?: 'ok' | 'warn' | 'bad' }) {
  return (
    <div className="rounded-lg border border-line bg-panel px-4 py-3">
      <div className="text-[11px] font-medium uppercase tracking-wide text-faint">{label}</div>
      <div className={cx('mt-1 text-2xl font-semibold tabular-nums', tone === 'ok' && 'text-ok', tone === 'warn' && 'text-warn', tone === 'bad' && 'text-bad')}>
        {value}
      </div>
      {hint && <div className="mt-0.5 text-xs text-muted">{hint}</div>}
    </div>
  );
}

export function Spinner({ label = 'Loading…' }: { label?: string }) {
  return (
    <div className="flex items-center gap-2 py-6 text-muted">
      <LoaderCircle className="size-4 animate-spin" /> {label}
    </div>
  );
}

export function Empty({ icon, title, children }: { icon?: ReactNode; title: string; children?: ReactNode }) {
  return (
    <div className="flex flex-col items-center justify-center gap-2 rounded-lg border border-dashed border-line px-6 py-12 text-center">
      {icon && <div className="text-faint">{icon}</div>}
      <div className="font-medium">{title}</div>
      {children && <div className="max-w-md text-[13px] text-muted">{children}</div>}
    </div>
  );
}

export function ErrorNote({ error }: { error: unknown }) {
  return (
    <div className="flex items-start gap-2 rounded-md border border-bad/40 bg-bad-soft px-3 py-2 text-[13px] text-bad">
      <CircleAlert className="mt-0.5 size-4 shrink-0" />
      <span>{errorText(error)}</span>
    </div>
  );
}

export function Meter({ value, tone = 'accent' }: { value: number; tone?: 'accent' | 'ok' | 'warn' }) {
  const pct = Math.max(0, Math.min(1, value)) * 100;
  const color = tone === 'ok' ? 'var(--ok)' : tone === 'warn' ? 'var(--warn)' : 'var(--accent)';
  // React applies `style` through the CSSOM, which the page's CSP allows
  // (it forbids only style attributes in markup).
  return (
    <span className="relative inline-block h-1.5 w-16 overflow-hidden rounded-full bg-raised align-middle">
      <span className="absolute inset-y-0 left-0 rounded-full" style={{ width: `${pct}%`, background: color }} />
    </span>
  );
}

// ─── Dialog ─────────────────────────────────────────────────────────────────

export function Dialog({
  open,
  title,
  onClose,
  children,
  footer,
  wide,
}: {
  open: boolean;
  title: ReactNode;
  onClose: () => void;
  children: ReactNode;
  footer?: ReactNode;
  wide?: boolean;
}) {
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => e.key === 'Escape' && onClose();
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [open, onClose]);
  if (!open) return null;
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/55 p-4" onMouseDown={onClose}>
      <div
        role="dialog"
        aria-modal="true"
        className={cx('flex max-h-[90vh] w-full flex-col rounded-xl border border-line bg-panel shadow-2xl', wide ? 'max-w-3xl' : 'max-w-lg')}
        onMouseDown={(e) => e.stopPropagation()}
      >
        <header className="flex items-center gap-2 border-b border-line px-5 py-3">
          <h2 className="text-[15px] font-semibold">{title}</h2>
          <Button tone="ghost" size="sm" className="ml-auto" onClick={onClose} aria-label="Close" icon={<X className="size-4" />} />
        </header>
        <div className="overflow-y-auto px-5 py-4">{children}</div>
        {footer && <footer className="flex justify-end gap-2 border-t border-line px-5 py-3">{footer}</footer>}
      </div>
    </div>
  );
}

// ─── Toasts ─────────────────────────────────────────────────────────────────

interface Toast {
  id: number;
  tone: 'ok' | 'bad' | 'info';
  text: string;
}

const ToastCtx = createContext<(tone: Toast['tone'], text: string) => void>(() => {});

export function ToastProvider({ children }: { children: ReactNode }) {
  const [toasts, setToasts] = useState<Toast[]>([]);
  const push = useCallback((tone: Toast['tone'], text: string) => {
    const id = Date.now() + Math.random();
    setToasts((t) => [...t, { id, tone, text }]);
    setTimeout(() => setToasts((t) => t.filter((x) => x.id !== id)), tone === 'bad' ? 8000 : 4000);
  }, []);
  return (
    <ToastCtx.Provider value={push}>
      {children}
      <div className="pointer-events-none fixed bottom-4 right-4 z-[60] flex w-96 max-w-[calc(100vw-2rem)] flex-col gap-2">
        {toasts.map((t) => {
          const Icon = t.tone === 'ok' ? CircleCheck : t.tone === 'bad' ? CircleX : Info;
          return (
            <div
              key={t.id}
              className={cx(
                'pointer-events-auto flex items-start gap-2 rounded-lg border bg-panel px-3 py-2.5 text-[13px] shadow-xl',
                t.tone === 'ok' && 'border-ok/40',
                t.tone === 'bad' && 'border-bad/40',
                t.tone === 'info' && 'border-info/40',
              )}
            >
              <Icon className={cx('mt-0.5 size-4 shrink-0', t.tone === 'ok' && 'text-ok', t.tone === 'bad' && 'text-bad', t.tone === 'info' && 'text-info')} />
              <span className="min-w-0 break-words">{t.text}</span>
            </div>
          );
        })}
      </div>
    </ToastCtx.Provider>
  );
}

export const useToast = () => useContext(ToastCtx);

// ─── Data loading ───────────────────────────────────────────────────────────

export interface Loaded<T> {
  data: T | undefined;
  error: unknown;
  loading: boolean;
  reload: () => void;
}

/** Runs `load` when `deps` change and on `reload()`; keeps the last data while reloading. */
export function useLoad<T>(load: () => Promise<T>, deps: unknown[]): Loaded<T> {
  const [data, setData] = useState<T>();
  const [error, setError] = useState<unknown>();
  const [loading, setLoading] = useState(true);
  const [tick, setTick] = useState(0);
  // biome-ignore lint/correctness/useExhaustiveDependencies: deps are the caller's
  useEffect(() => {
    let live = true;
    setLoading(true);
    load()
      .then((d) => {
        if (!live) return;
        setData(d);
        setError(undefined);
      })
      .catch((e) => live && setError(e))
      .finally(() => live && setLoading(false));
    return () => {
      live = false;
    };
  }, [...deps, tick]);
  const reload = useCallback(() => setTick((t) => t + 1), []);
  return { data, error, loading, reload };
}

export function useInterval(fn: () => void, ms: number | null) {
  const saved = useRef(fn);
  saved.current = fn;
  useEffect(() => {
    if (ms === null) return;
    const id = setInterval(() => saved.current(), ms);
    return () => clearInterval(id);
  }, [ms]);
}
