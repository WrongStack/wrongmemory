// The page talks only to the CLI process that served it. Reads are GET
// requests; changes are POST requests carrying the run's CSRF token, which
// the server hands out only to a request with the session cookie.

export interface ApiError {
  code: string;
  message: string;
}

export function errorText(e: unknown): string {
  if (e && typeof e === 'object' && 'message' in e) {
    const err = e as Partial<ApiError>;
    return err.code ? `${err.code}: ${err.message}` : String(err.message);
  }
  return String(e);
}

const SESSION_EXPIRED: ApiError = {
  code: 'UNAUTHORIZED',
  message: 'Session expired: run `wrongmemory ui` again and open its new link.',
};

async function unwrap<T>(res: Response): Promise<T> {
  if (res.status === 401) throw SESSION_EXPIRED;
  if (res.status === 403) throw { code: 'FORBIDDEN', message: await res.text() } satisfies ApiError;
  const body = await res.json();
  if (!body.ok) throw body.error as ApiError;
  return body.result as T;
}

export async function read<T>(m: string, p?: string, a?: object): Promise<T> {
  const q = new URLSearchParams({ m });
  if (p) q.set('p', p);
  if (a) q.set('a', JSON.stringify(a));
  return unwrap<T>(await fetch(`/api?${q.toString()}`, { credentials: 'same-origin' }));
}

let csrf: Promise<string> | null = null;

function token(): Promise<string> {
  csrf ??= fetch('/session', { credentials: 'same-origin' }).then(async (res) => {
    if (res.status === 401) throw SESSION_EXPIRED;
    const body = (await res.json()) as { csrf: string };
    return body.csrf;
  });
  return csrf;
}

export async function change<T>(m: string, p: string, a: object = {}): Promise<T> {
  const res = await fetch('/api', {
    method: 'POST',
    credentials: 'same-origin',
    headers: { 'Content-Type': 'application/json', 'X-WrongMemory-CSRF': await token() },
    body: JSON.stringify({ m, p, a }),
  });
  return unwrap<T>(res);
}
