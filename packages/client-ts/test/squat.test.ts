import { expect, test } from 'bun:test';
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { WrongMemoryClient } from '../src/client.ts';

// A process squatting the endpoint name cannot produce the HMAC proof, so the
// client must refuse before writing any credential. On Unix the squatter
// listens on the socket path that discovery names.
test('squatted endpoint never receives a credential', async () => {
  const root = mkdtempSync(join(tmpdir(), 'wm-squat-'));
  mkdirSync(join(root, 'runtime'), { recursive: true });
  mkdirSync(join(root, 'grants'), { recursive: true });
  const endpoint =
    process.platform === 'win32'
      ? `\\\\.\\pipe\\wm-squat-test-${process.pid}-${Date.now()}`
      : join(root, 'runtime', 'daemon.sock');
  const received: string[] = [];
  const server = createServer((sock) => {
    sock.on('data', (d) => {
      received.push(d.toString('utf8'));
      // Pretend to be the daemon but without the token.
      sock.write(`${JSON.stringify({ type: 'proof', proof: '00'.repeat(32) })}\n`);
    });
  });
  await new Promise<void>((r) => server.listen(endpoint, r));
  writeFileSync(
    join(root, 'runtime', 'endpoint.json'),
    JSON.stringify({
      endpoint,
      daemonInstanceId: 'd_fake',
      daemonVersion: '0',
      pid: process.pid,
      protocolMajor: 1,
      protocolMinor: 0,
      startedAtMs: 0,
    }),
  );
  writeFileSync(join(root, 'runtime', 'token'), 'f'.repeat(64));
  writeFileSync(join(root, 'grants', 'g_test.secret'), 'grant-secret-must-not-leak');
  try {
    for (const grantId of [undefined, 'g_test']) {
      const err = await WrongMemoryClient.connect({ dataRoot: root, grantId, timeoutMs: 2000 }).catch(
        (e) => e,
      );
      expect(err?.code).toBe('UNAVAILABLE');
    }
    const all = received.join('');
    expect(all).toContain('"challenge"');
    expect(all).not.toContain('credential');
    expect(all).not.toContain('grant-secret-must-not-leak');
    expect(all).not.toContain('f'.repeat(64));
  } finally {
    server.close();
  }
});
