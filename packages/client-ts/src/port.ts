/**
 * Skeleton WrongStack `MemoryPort` adapter over the WrongMemory daemon
 * (WM-400, client half). It mirrors the capability IDs and SAGE service method
 * names from `packages/sage/src/memory-port.ts` / `service-contract.ts` at the
 * baseline commit.
 *
 * Methods the daemon supports today are mapped to IPC calls. Every other
 * method throws `WrongMemoryError('NOT_IMPLEMENTED')`. The adapter NEVER falls
 * back to opening local SQLite (wrongstack-integration §9).
 */

import type { WrongMemoryClient } from './client.ts';
import {
  type CandidateListResult,
  type MemoryCandidate,
  METHODS,
  type MutationOutcome,
  type ProposeParams,
  type RememberParams,
  type SageRecord,
  type SearchResult,
  type SessionInfo,
} from './contracts.ts';
import { isWrongMemoryError, WrongMemoryError } from './errors.ts';

export const SAGE_SERVICE_CAPABILITY_ID = 'wrongstack.memory.sage-service.v1';
export const SAGE_RETRIEVAL_CAPABILITY_ID = 'wrongstack.memory.retrieval.v1';
export const SAGE_SURFACE_CAPABILITY_ID = 'wrongstack.memory.surface.v1';

export interface MemoryCapabilityLike {
  id: string;
}

export interface SessionOpts {
  sessionId?: string | undefined;
  /** Admin bypass; not exposed by the daemon to adapters (always rejected). */
  includeAllSessions?: boolean | undefined;
}

/** Subset of `RememberSageInput` accepted by the daemon today. */
export type RememberSageInput = RememberParams & { ownerSessionId?: string | undefined };
/** Subset of `CreateCandidateInput` accepted by the daemon today. */
export type CreateCandidateInput = ProposeParams & { ownerSessionId?: string | undefined };

export interface WrongMemorySageService {
  searchSage(
    query: string,
    opts?: { limit?: number; includeStatuses?: string[] } & SessionOpts,
  ): Promise<SageRecord[]>;
  getSage(id: string): Promise<SageRecord | null>;
  retrieveForPath(
    opts: { path: string; limit?: number; includeAncestors?: boolean; includeStatuses?: string[] } & SessionOpts,
  ): Promise<SageRecord[]>;
  rememberSage(input: RememberSageInput): Promise<SageRecord>;
  createCandidate(input: CreateCandidateInput): Promise<MemoryCandidate>;
  listCandidates(includeResolved?: boolean): Promise<MemoryCandidate[]>;
  [unmapped: string]: unknown;
}

/** SAGE service methods that have no daemon operation yet. */
export const UNMAPPED_SERVICE_METHODS = [
  'unifiedSearchService',
  'searchSageWithBreakdown',
  'retrieveForAudience',
  'findRelatedSage',
  'graphFor',
  'verify',
  'verifyForPaths',
  'hygiene',
  'resolveCandidate',
  'acceptCandidate',
  'rejectCandidate',
  'updateSage',
  'deleteSage',
  'recoverSage',
  'backfillRecoverable',
  'findMemoriesForFile',
  'listSagePage',
  'recordInjection',
  'recordUse',
  'getHqSyncVersion',
  'listHqSync',
  'applyHqSync',
] as const;

/** Legacy `MemoryStore` surface; not mapped yet. */
export const UNMAPPED_STORE_METHODS = [
  'readAll',
  'read',
  'remember',
  'forget',
  'consolidate',
  'clear',
  'list',
  'search',
  'findRelated',
  'scoreRelevant',
] as const;

function notImplemented(name: string): never {
  throw new WrongMemoryError(
    'NOT_IMPLEMENTED',
    `${name} is not supported by the WrongMemory daemon yet (no local SQLite fallback)`,
  );
}

const STATUSES_SERVED_BY_FOR_PATH = new Set(['active', 'stale']);

export interface WrongMemoryPortOptions {
  client: WrongMemoryClient;
  projectId: string;
}

export interface WrongMemoryPort {
  initialize(): Promise<void>;
  getCapability<T>(capability: MemoryCapabilityLike): T | undefined;
  health(): Promise<{ status: 'ready' | 'unavailable'; backend: string; details?: Record<string, unknown> }>;
  dispose(): Promise<void>;
  withTraceId(traceId: string): WrongMemoryPort;
  readonly service: WrongMemorySageService;
  [legacy: string]: unknown;
}

export function createWrongMemoryPort(opts: WrongMemoryPortOptions): WrongMemoryPort {
  const { client, projectId } = opts;
  let boundSession: string | undefined;

  /** Bind the host session that owns session-scoped records (trusted host only). */
  async function bindSession(sessionOpts: SessionOpts | undefined): Promise<void> {
    if (sessionOpts?.includeAllSessions) {
      throw new WrongMemoryError(
        'FORBIDDEN',
        'includeAllSessions is an administrative capability and is not available to adapters',
      );
    }
    const wanted = sessionOpts?.sessionId;
    if (wanted && wanted !== boundSession) {
      const info = await client.call<SessionInfo>(METHODS.SESSION_ACQUIRE, { hostSessionId: wanted });
      boundSession = info.sessionId;
    }
  }

  const call = <T>(method: string, params: unknown, idempotencyKey?: string) =>
    client.call<T>(method, params, idempotencyKey ? { projectId, idempotencyKey } : { projectId });

  const mapped = {
    async searchSage(query: string, o: { limit?: number; includeStatuses?: string[] } & SessionOpts = {}) {
      await bindSession(o);
      const params: Record<string, unknown> = { query, limit: o.limit ?? 20 };
      if (o.includeStatuses) params.statuses = o.includeStatuses;
      const r = await call<SearchResult>(METHODS.MEMORY_SEARCH, params);
      return r.hits.map((h) => h.memory);
    },
    async getSage(id: string) {
      try {
        return await call<SageRecord>(METHODS.MEMORY_GET, { id });
      } catch (e) {
        if (isWrongMemoryError(e, 'NOT_FOUND')) return null;
        throw e;
      }
    },
    async retrieveForPath(
      o: { path: string; limit?: number; includeAncestors?: boolean; includeStatuses?: string[] } & SessionOpts,
    ) {
      await bindSession(o);
      if (o.includeStatuses?.some((s) => !STATUSES_SERVED_BY_FOR_PATH.has(s))) {
        notImplemented('retrieveForPath(includeStatuses beyond active/stale)');
      }
      const r = await call<SearchResult>(METHODS.MEMORY_FOR_PATH, { path: o.path, limit: o.limit ?? 20 });
      return r.hits
        .filter((h) => o.includeAncestors !== false || !(h.reasons ?? []).some((x) => x.startsWith('ancestor')))
        .map((h) => h.memory);
    },
    async rememberSage(input: RememberSageInput) {
      const { ownerSessionId, ...params } = input;
      if (params.scope === 'session') await bindSession({ sessionId: ownerSessionId });
      const r = await call<MutationOutcome>(METHODS.MEMORY_REMEMBER, params);
      return r.record as SageRecord;
    },
    async createCandidate(input: CreateCandidateInput) {
      const { ownerSessionId, ...params } = input;
      if (params.scope === 'session') await bindSession({ sessionId: ownerSessionId });
      const r = await call<MutationOutcome>(METHODS.CANDIDATE_PROPOSE, params);
      return r.record as MemoryCandidate;
    },
    async listCandidates(includeResolved = false) {
      const statuses = includeResolved ? ['pending', 'accepted', 'rejected', 'merged'] : ['pending'];
      const out: MemoryCandidate[] = [];
      for (const status of statuses) {
        const r = await call<CandidateListResult>(METHODS.CANDIDATE_LIST, { status, limit: 200 });
        out.push(...r.candidates);
      }
      return out;
    },
  };

  const service: WrongMemorySageService = { ...mapped };
  for (const name of UNMAPPED_SERVICE_METHODS) {
    service[name] = () => notImplemented(name);
  }

  const port: WrongMemoryPort = {
    service,
    async initialize() {
      await client.call(METHODS.PROJECT_GET, {}, { projectId });
    },
    getCapability<T>(capability: MemoryCapabilityLike): T | undefined {
      if (capability.id === SAGE_SERVICE_CAPABILITY_ID) return service as unknown as T;
      // Retrieval (automatic injection) and surface (WebUI) capabilities need
      // operations the daemon does not provide yet; advertise them absent
      // rather than half-working.
      return undefined;
    },
    async health() {
      try {
        await client.health();
        return { status: 'ready', backend: 'wrongmemory' };
      } catch (e) {
        return {
          status: 'unavailable',
          backend: 'wrongmemory',
          details: { error: e instanceof Error ? e.message : String(e) },
        };
      }
    },
    async dispose() {
      client.close();
    },
    withTraceId() {
      // Trace correlation is not part of the wire protocol yet.
      return port;
    },
  };
  for (const name of UNMAPPED_STORE_METHODS) {
    port[name] = () => notImplemented(name);
  }
  return port;
}
