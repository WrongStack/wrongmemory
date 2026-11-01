/**
 * Integration against the real `wrongmemoryd` binary built by cargo
 * (`target/debug/wrongmemoryd.exe`); nothing is mocked.
 */

import { afterAll, beforeAll, describe, expect, test } from 'bun:test';
import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import {
  createWrongMemoryPort,
  type GrantCreateReply,
  isWrongMemoryError,
  METHODS,
  type MutationOutcome,
  type RegisterProjectResult,
  type SearchResult,
  WrongMemoryClient,
  WrongMemoryError,
} from '../src/index.ts';

const repo = resolve(import.meta.dir, '../../..');
const daemonPath = join(repo, 'target', 'debug', 'wrongmemoryd.exe');
const isWindows = process.platform === 'win32';

let tmp = '';
let dataRoot = '';
let host: WrongMemoryClient;
let projectA = '';
let projectB = '';

async function expectCode(p: Promise<unknown>, code: string) {
  try {
    await p;
  } catch (e) {
    expect(isWrongMemoryError(e)).toBe(true);
    expect((e as WrongMemoryError).code).toBe(code as WrongMemoryError['code']);
    return;
  }
  throw new Error(`expected ${code}`);
}

describe.skipIf(!isWindows)('wrongmemoryd integration', () => {
  beforeAll(async () => {
    if (!existsSync(daemonPath)) {
      execFileSync('cargo', ['build', '-p', 'wrongmemoryd'], { cwd: repo, stdio: 'inherit' });
    }
    tmp = mkdtempSync(join(tmpdir(), 'wm-client-ts-'));
    dataRoot = join(tmp, 'home');
    for (const p of ['A', 'B']) mkdirSync(join(tmp, p));
    host = await WrongMemoryClient.connect({ dataRoot, start: true, daemonPath, clientName: 'ts-test' });
    const reg = (name: string) =>
      host.call<RegisterProjectResult>(METHODS.PROJECT_REGISTER, {
        root: join(tmp, name),
        storeKind: 'managed',
        write: true,
      });
    projectA = (await reg('A')).project.projectId;
    projectB = (await reg('B')).project.projectId;
  }, 60_000);

  afterAll(async () => {
    if (host) {
      try {
        await host.call(METHODS.DAEMON_SHUTDOWN);
      } catch {
        /* already gone */
      }
      host.close();
    }
    // Give the daemon time to release files before removing the temp root.
    await new Promise((r) => setTimeout(r, 500));
    try {
      rmSync(tmp, { recursive: true, force: true });
    } catch {
      /* best effort */
    }
  });

  test('host: hello, remember with idempotency, search', async () => {
    expect(host.hello.principal.kind).toBe('host');
    expect(host.hello.storageProfileId).toBe('sage-sqlite-v5-baseline');
    const params = { text: 'Release builds use cargo build --locked', kind: 'convention' };
    const a = await host.call<MutationOutcome>(METHODS.MEMORY_REMEMBER, params, {
      projectId: projectA,
      idempotencyKey: 'ts-k1',
    });
    const b = await host.call<MutationOutcome>(METHODS.MEMORY_REMEMBER, params, {
      projectId: projectA,
      idempotencyKey: 'ts-k1',
    });
    expect(a.outcome).toBe('memory-committed');
    expect(b.replayed).toBe(true);
    expect(b.id).toBe(a.id);
    const r = await host.call<SearchResult>(METHODS.MEMORY_SEARCH, { query: 'cargo locked' }, { projectId: projectA });
    expect(r.hits.map((h) => h.memory.id)).toEqual([a.id]);
    // Project B never sees A's memory.
    const rb = await host.call<SearchResult>(METHODS.MEMORY_SEARCH, { query: 'cargo locked' }, { projectId: projectB });
    expect(rb.hits).toEqual([]);
  });

  test('grant: propose allowed, remember forbidden, ungranted project hidden', async () => {
    const g = await host.call<GrantCreateReply>(METHODS.GRANT_CREATE, {
      label: 'ts-agent',
      profile: 'read-propose',
      projects: [projectA],
    });
    const ext = await WrongMemoryClient.connect({ dataRoot, grantId: g.grant.grantId, clientName: 'ts-agent' });
    try {
      expect(ext.hello.principal.kind).toBe('grant');
      expect(ext.hello.principal.profile).toBe('read-propose');
      const p = await ext.call<MutationOutcome>(
        METHODS.CANDIDATE_PROPOSE,
        { text: 'CI runs on windows-latest' },
        { projectId: projectA },
      );
      expect(p.outcome).toBe('candidate-pending');
      expect(p.id.startsWith('candidate_')).toBe(true);
      await expectCode(ext.call(METHODS.MEMORY_REMEMBER, { text: 'forged' }, { projectId: projectA }), 'FORBIDDEN');
      await expectCode(ext.call(METHODS.MEMORY_SEARCH, { query: 'x' }, { projectId: projectB }), 'PROJECT_NOT_FOUND');
      await expectCode(ext.call(METHODS.GRANT_LIST), 'FORBIDDEN');
      // The connection stays usable after typed errors.
      const ok = await ext.call<SearchResult>(METHODS.MEMORY_SEARCH, { query: 'cargo' }, { projectId: projectA });
      expect(ok.hits.length).toBe(1);
    } finally {
      ext.close();
    }
    await expectCode(
      WrongMemoryClient.connect({ dataRoot, grantId: 'g_doesnotexist' }),
      'UNAVAILABLE',
    );
  });

  test('port skeleton maps supported calls and refuses the rest', async () => {
    const port = createWrongMemoryPort({ client: host, projectId: projectA });
    await port.initialize();
    expect((await port.health()).status).toBe('ready');
    const svc = port.getCapability<typeof port.service>({ id: 'wrongstack.memory.sage-service.v1' });
    expect(svc).toBeDefined();
    expect(port.getCapability({ id: 'wrongstack.memory.retrieval.v1' })).toBeUndefined();
    const rec = await port.service.rememberSage({
      text: 'Deploy script lives in scripts/deploy.ts',
      anchors: [{ type: 'file', path: 'scripts/deploy.ts' }],
    });
    expect(await port.service.getSage(rec.id)).toMatchObject({ id: rec.id, revision: 1 });
    expect(await port.service.getSage('01NOTTHERE')).toBeNull();
    const found = await port.service.searchSage('deploy script');
    expect(found.map((m) => m.id)).toContain(rec.id);
    const byPath = await port.service.retrieveForPath({ path: 'scripts/deploy.ts' });
    expect(byPath.map((m) => m.id)).toContain(rec.id);
    const cand = await port.service.createCandidate({ text: 'Deploys require a green CI run' });
    const pending = await port.service.listCandidates();
    expect(pending.map((c) => c.id)).toContain(cand.id);
    // Session-scoped write is owned by the bound host session.
    const priv = await port.service.rememberSage({ text: 'private scratch note', scope: 'session', ownerSessionId: 'sess-ts' });
    expect(priv.ownerSessionId).toBe('sess-ts');
    expect(() => (port.service.updateSage as () => void)()).toThrow(/NOT_IMPLEMENTED/);
    expect(() => (port.readAll as () => void)()).toThrow(/NOT_IMPLEMENTED/);
    await expectCode(port.service.searchSage('x', { includeAllSessions: true }), 'FORBIDDEN');
  });
});
