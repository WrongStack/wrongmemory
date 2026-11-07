// Shapes returned by the daemon (see crates/wrongmemory-contracts). Only the
// fields the dashboard reads are listed; records keep unknown fields.

export interface Project {
  projectId: string;
  displayName?: string;
  root: string;
  storePath: string;
  storeKind: 'adopted' | 'managed' | string;
  admission: 'read-write' | 'read-only' | string;
  admissionReason?: string;
  ownerEpoch?: number;
  createdAtMs?: number;
}

export interface Stats {
  byStatus?: Record<string, number>;
  byKind?: Record<string, number>;
  pendingCandidates?: number;
  edges?: number;
}

export interface Health {
  daemonInstanceId: string;
  daemonVersion: string;
  pid: number;
  uptimeMs: number;
  dataRoot: string;
  sqliteVersion: string;
  projects: number;
  openStores: number;
  connections: number;
  draining: boolean;
}

export interface Anchor {
  type: string;
  path?: string;
  symbol?: string;
  command?: string;
  role?: string;
}

export interface Source {
  type: string;
  sessionId?: string;
  path?: string;
  command?: string;
  [k: string]: unknown;
}

export interface Validity {
  statement: string;
  checks?: { kind: string; path: string; text: string }[];
}

export interface Memory {
  id: string;
  revision: number;
  text: string;
  kind: string;
  scope: string;
  status: string;
  tags: string[];
  anchors: Anchor[];
  sources: Source[];
  validity?: Validity;
  importance: number;
  confidence: number;
  freshness?: number;
  persistence?: string;
  supersedes?: string[];
  contradicts?: string[];
  supersededBy?: string;
  createdAt: string;
  updatedAt: string;
  ownerSessionId?: string;
  expiresAt?: string;
  contextPolicy?: string;
  summary?: string;
  feedback?: { verdict?: string; note?: string; at?: string; [k: string]: unknown }[];
  lastAccessedAt?: string;
  lastVerifiedAt?: string;
  staleReason?: string;
  lastUsedAt?: string;
  injectionCount?: number;
  useCount?: number;
  wrongmemory?: { principalId?: string; client?: string };
  [k: string]: unknown;
}

export interface Candidate {
  id: string;
  status: string;
  text: string;
  kind: string;
  scope: string;
  confidence: number;
  importance: number;
  tags: string[];
  anchors: Anchor[];
  sources: Source[];
  createdAt: string;
  updatedAt: string;
  memoryId?: string;
  reason?: string;
  targetMemoryId?: string;
  targetRevision?: number;
  reviewReason?: string;
  suggestedAction?: string;
  supersedesMemoryIds?: string[];
}

export interface GraphEdge {
  from: string;
  to: string;
  relation: string;
  weight: number;
  createdAt: string;
}

export interface GraphView {
  edges: GraphEdge[];
  memories: Memory[];
  truncated: boolean;
}

export interface Job {
  jobId: string;
  kind: string;
  state: string;
  createdAtMs?: number;
  startedAtMs?: number;
  finishedAtMs?: number;
  error?: string;
  result?: unknown;
}

export interface Schedule {
  enabled: boolean;
  cadenceMs: number;
  nextDueMs?: number;
  lastRunMs?: number;
  lastOutcome?: string;
}

export interface DoctorCheck {
  id: string;
  title?: string;
  severity: 'ok' | 'info' | 'warn' | 'error' | string;
  summary?: string;
  fix?: string;
  details?: { fix?: string; samples?: unknown[] };
}

export interface DoctorReport {
  generatedAt?: string;
  checks: DoctorCheck[];
  metrics?: Record<string, unknown>;
  fixesApplied?: unknown[];
}

export interface Grant {
  grantId: string;
  label: string;
  profile: string;
  projects: string[];
  createdAtMs?: number;
  expiresAtMs?: number;
  revokedAtMs?: number;
}

export interface ChangeEvent {
  generation?: number;
  change: string;
  memoryId?: string;
  candidateId?: string;
  revision?: number;
  atMs?: number;
  origin?: string;
  telemetry?: boolean;
}

export interface AuditRecord {
  event: string;
  memoryId?: string;
  reason?: string;
  at?: string;
  createdAt?: string;
  details?: unknown;
}
