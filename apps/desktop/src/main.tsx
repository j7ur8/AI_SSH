import React, { useEffect, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import {
  Activity,
  Eye,
  EyeOff,
  FolderKey,
  FolderOpen,
  History,
  KeyRound,
  Plus,
  RefreshCw,
  Save,
  Settings,
  ShieldAlert,
  TerminalSquare,
  Trash2,
} from "lucide-react";
import "@xterm/xterm/css/xterm.css";
import "./styles.css";

type SessionStatus =
  | "connecting"
  | "ready"
  | "exec_running"
  | "pty_open"
  | "idle"
  | "disconnected"
  | "interrupted"
  | "closed"
  | "failed";

type Session = {
  id: string;
  target_id: string;
  target_name: string;
  purpose: string;
  client_name: string;
  status: SessionStatus;
  current_command: string | null;
  last_exit_code: number | null;
  recording_truncated: boolean;
  host_fingerprint: string | null;
  created_at: string;
  updated_at: string;
  closed_at: string | null;
};

type TerminalEvent = {
  command_id: string | null;
  sequence: number;
  stream: "stdout" | "stderr" | "pty" | "system";
  timestamp: string;
  payload: number[];
};

type CommandStatus = "running" | "completed" | "failed" | "timed_out" | "cancelled";

type SessionCommand = {
  id: string;
  session_id: string;
  command: string;
  status: CommandStatus;
  exit_code: number | null;
  started_at: string;
  finished_at: string | null;
  last_sequence: number;
  recording_truncated: boolean;
};

type Auth =
  | { type: "password"; password: string }
  | { type: "private_key"; path: string; passphrase: string | null };

type TargetConfig = {
  id: string;
  name: string;
  host: string;
  port: number;
  username: string;
  auth: Auth;
};

type AppConfig = {
  version: number;
  retention_days: number;
  idle_timeout_seconds: number;
  connect_timeout_seconds: number;
  keepalive_seconds: number;
  recording_limit_mib: number;
  quit_daemon_on_app_exit: boolean;
  launch_at_login: boolean;
  targets: TargetConfig[];
};

type ResponseData =
  | { kind: "sessions"; data: Session[] }
  | { kind: "session"; data: Session }
  | {
      kind: "events";
      data: {
        command?: SessionCommand | null;
        commands?: SessionCommand[];
        events: TerminalEvent[];
        next_sequence: number;
        has_more: boolean;
      };
    };

function App() {
  return <Dashboard />;
}

function Dashboard() {
  const [view, setView] = useState<"sessions" | "configuration">("sessions");
  const [sessions, setSessions] = useState<Session[]>([]);
  const [config, setConfig] = useState<AppConfig | null>(null);
  const [error, setError] = useState("");
  const [connectionError, setConnectionError] = useState("");
  const [busy, setBusy] = useState(false);
  const [selectedSessionId, setSelectedSessionId] = useState<string | null>(null);

  const loadSessions = async () => {
    try {
      const value = await invoke<ResponseData>("sessions", { includeHistory: true });
      if (value.kind === "sessions") {
        setSessions(value.data);
        setSelectedSessionId((current) => current && value.data.some((item) => item.id === current)
          ? current
          : value.data[0]?.id ?? null);
        setConnectionError("");
      }
    } catch (cause) {
      setConnectionError(String(cause));
    }
  };

  const loadConfig = async () => {
    try {
      const value = await invoke<AppConfig>("config_get");
      setConfig(value);
      if (value.targets.length === 0) setView("configuration");
    } catch (cause) {
      setError(String(cause));
    }
  };

  useEffect(() => {
    void loadSessions();
    void loadConfig();
    const timer = window.setInterval(loadSessions, 2000);
    let unlisten: (() => void) | undefined;
    void listen<string>("select-session", (event) => {
      setView("sessions");
      setSelectedSessionId(event.payload);
      void loadSessions();
    }).then((stop) => { unlisten = stop; });
    return () => {
      window.clearInterval(timer);
      unlisten?.();
    };
  }, []);

  const testTarget = async (targetId: string) => {
    setBusy(true);
    try {
      await invoke("test_target", { targetId });
      setError("");
      await loadSessions();
    } catch (cause) {
      setError(String(cause));
    } finally {
      setBusy(false);
    }
  };

  return (
    <main className={`page page-${view}`}>
      <Header>
        <nav className="tabs" aria-label="Views">
          <button className={view === "sessions" ? "active" : ""} onClick={() => setView("sessions")}>
            <History size={16} /> Sessions
          </button>
          <button
            className={view === "configuration" ? "active" : ""}
            onClick={() => setView("configuration")}
          >
            <Settings size={16} /> Configuration
          </button>
        </nav>
      </Header>
      {(error || connectionError) && <div className="error-banner">{error || connectionError}</div>}
      {view === "sessions" ? (
        <SessionsView
          sessions={sessions}
          selectedSessionId={selectedSessionId}
          busy={busy}
          onSelect={setSelectedSessionId}
          onRefresh={loadSessions}
        />
      ) : config ? (
        <ConfigEditor
          initial={config}
          busy={busy}
          onSaved={(saved) => {
            setConfig(saved);
            setError("");
          }}
          onError={setError}
          onTest={testTarget}
        />
      ) : (
        <div className="loading">Loading configuration...</div>
      )}
    </main>
  );
}

function SessionsView({
  sessions,
  selectedSessionId,
  busy,
  onSelect,
  onRefresh,
}: {
  sessions: Session[];
  selectedSessionId: string | null;
  busy: boolean;
  onSelect: (sessionId: string) => void;
  onRefresh: () => Promise<void>;
}) {
  const selected = sessions.find((item) => item.id === selectedSessionId) ?? null;
  return (
    <section className="sessions-workspace">
      <aside className="sessions-pane">
        <div className="sessions-pane-header">
          <div>
            <h2>Sessions</h2>
            <span>{sessions.length} recorded</span>
          </div>
          <button className="icon" title="Refresh sessions" onClick={() => void onRefresh()}>
            <RefreshCw size={17} />
          </button>
        </div>
        <div className="session-list-scroll">
          {sessions.map((item) => (
            <button
              key={item.id}
              className={`session-row ${item.id === selectedSessionId ? "active" : ""}`}
              aria-pressed={item.id === selectedSessionId}
              onClick={() => onSelect(item.id)}
            >
              <span className="session-row-heading">
                <strong>{item.target_name}</strong>
                <Status value={item.status} />
              </span>
              <code>{item.current_command || item.purpose || "No command"}</code>
              <span className="session-row-meta">
                <span>{item.client_name}</span>
                <time>{new Date(item.created_at).toLocaleString()}</time>
              </span>
            </button>
          ))}
          {!sessions.length && !busy && <div className="session-list-empty">No sessions recorded</div>}
        </div>
      </aside>
      {selected ? (
        <SessionDetail key={selected.id} id={selected.id} />
      ) : (
        <section className="session-detail-empty">
          <TerminalSquare size={24} />
          <span>No session selected</span>
        </section>
      )}
    </section>
  );
}

function ConfigEditor({
  initial,
  busy,
  onSaved,
  onError,
  onTest,
}: {
  initial: AppConfig;
  busy: boolean;
  onSaved: (config: AppConfig) => void;
  onError: (message: string) => void;
  onTest: (targetId: string) => Promise<void>;
}) {
  const [draft, setDraft] = useState<AppConfig>(() => structuredClone(initial));
  const [selected, setSelected] = useState(initial.targets[0]?.id ?? "");
  const [saving, setSaving] = useState(false);
  const [showSecret, setShowSecret] = useState(false);
  const targetIndex = draft.targets.findIndex((target) => target.id === selected);
  const target = targetIndex >= 0 ? draft.targets[targetIndex] : null;

  useEffect(() => {
    setDraft(structuredClone(initial));
    setSelected((current) => initial.targets.some((target) => target.id === current)
      ? current
      : initial.targets[0]?.id ?? "");
  }, [initial]);

  const updateGlobal = (field: keyof AppConfig, value: number) => {
    setDraft((current) => ({ ...current, [field]: value }));
  };

  const updateTarget = (patch: Partial<TargetConfig>) => {
    if (targetIndex < 0) return;
    setDraft((current) => ({
      ...current,
      targets: current.targets.map((item, index) => index === targetIndex ? { ...item, ...patch } : item),
    }));
    if (patch.id) setSelected(patch.id);
  };

  const updateAuth = (patch: Partial<Auth>) => {
    if (!target) return;
    updateTarget({ auth: { ...target.auth, ...patch } as Auth });
  };

  const addTarget = () => {
    let suffix = draft.targets.length + 1;
    while (draft.targets.some((target) => target.id === `target-${suffix}`)) suffix += 1;
    const next: TargetConfig = {
      id: `target-${suffix}`,
      name: `Target ${suffix}`,
      host: "",
      port: 22,
      username: "",
      auth: { type: "password", password: "" },
    };
    setDraft((current) => ({ ...current, targets: [...current.targets, next] }));
    setSelected(next.id);
  };

  const removeTarget = (id: string) => {
    const remaining = draft.targets.filter((target) => target.id !== id);
    setDraft((current) => ({ ...current, targets: remaining }));
    if (selected === id) setSelected(remaining[0]?.id ?? "");
  };

  const save = async (): Promise<boolean> => {
    setSaving(true);
    try {
      await invoke("config_save", { config: draft });
      onSaved(structuredClone(draft));
      return true;
    } catch (cause) {
      onError(String(cause));
      return false;
    } finally {
      setSaving(false);
    }
  };

  return (
    <section className="config-view">
      <div className="config-toolbar">
        <div>
          <h2>Configuration</h2>
          <span>{draft.targets.length} targets</span>
        </div>
        <div className="toolbar-actions">
          <button className="secondary" onClick={() => invoke("open_config")}>
            <FolderOpen size={16} /> Config directory
          </button>
          <button className="primary" disabled={saving} onClick={() => void save()}>
            <Save size={16} /> {saving ? "Saving" : "Save"}
          </button>
        </div>
      </div>

      <div className="app-settings">
        <label className="check-setting">
          <input
            type="checkbox"
            checked={draft.launch_at_login}
            onChange={(event) => setDraft((current) => ({
              ...current,
              launch_at_login: event.target.checked,
            }))}
          />
          <span>Launch at login</span>
        </label>
        <label className="check-setting">
          <input
            type="checkbox"
            checked={draft.quit_daemon_on_app_exit}
            onChange={(event) => setDraft((current) => ({
              ...current,
              quit_daemon_on_app_exit: event.target.checked,
            }))}
          />
          <span>Stop daemon when quitting</span>
        </label>
      </div>

      <div className="runtime-settings">
        <NumberField label="Retention (days)" min={1} max={3650} value={draft.retention_days} onChange={(v) => updateGlobal("retention_days", v)} />
        <NumberField label="Idle timeout (sec)" min={1} value={draft.idle_timeout_seconds} onChange={(v) => updateGlobal("idle_timeout_seconds", v)} />
        <NumberField label="Connect timeout (sec)" min={1} value={draft.connect_timeout_seconds} onChange={(v) => updateGlobal("connect_timeout_seconds", v)} />
        <NumberField label="Keepalive (sec)" min={1} value={draft.keepalive_seconds} onChange={(v) => updateGlobal("keepalive_seconds", v)} />
        <NumberField label="Recording limit (MiB)" min={1} value={draft.recording_limit_mib} onChange={(v) => updateGlobal("recording_limit_mib", v)} />
      </div>

      <div className="target-editor">
        <aside className="target-list">
          <div className="target-list-header">
            <span>Targets</span>
            <button className="icon" title="Add target" onClick={addTarget}><Plus size={16} /></button>
          </div>
          <div className="target-list-scroll">
            {draft.targets.map((item) => (
              <button
                key={item.id}
                className={`target-row ${selected === item.id ? "active" : ""}`}
                onClick={() => setSelected(item.id)}
              >
                <span><strong>{item.name || "Unnamed"}</strong><small>{item.host || item.id}</small></span>
                <Trash2
                  size={15}
                  role="button"
                  aria-label={`Delete ${item.name}`}
                  onClick={(event) => { event.stopPropagation(); removeTarget(item.id); }}
                />
              </button>
            ))}
            {!draft.targets.length && <div className="target-empty">No targets</div>}
          </div>
        </aside>

        <div className="target-form">
          {target ? (
            <>
              <div className="form-grid">
                <TextField label="Display name" value={target.name} onChange={(name) => updateTarget({ name })} />
                <TextField label="Target ID" value={target.id} onChange={(id) => updateTarget({ id })} />
                <TextField label="Host" value={target.host} onChange={(host) => updateTarget({ host })} />
                <NumberField label="Port" min={1} max={65535} value={target.port} onChange={(port) => updateTarget({ port })} />
                <TextField label="Username" value={target.username} onChange={(username) => updateTarget({ username })} />
              </div>

              <div className="auth-section">
                <div className="auth-heading"><KeyRound size={16} /><span>Authentication</span></div>
                <div className="segmented">
                  <button
                    className={target.auth.type === "password" ? "active" : ""}
                    onClick={() => updateTarget({ auth: { type: "password", password: "" } })}
                  >Password</button>
                  <button
                    className={target.auth.type === "private_key" ? "active" : ""}
                    onClick={() => updateTarget({ auth: { type: "private_key", path: "", passphrase: null } })}
                  >Private key</button>
                </div>
                {target.auth.type === "password" ? (
                  <SecretField label="Password" shown={showSecret} value={target.auth.password} onToggle={() => setShowSecret((value) => !value)} onChange={(password) => updateAuth({ password })} />
                ) : (
                  <div className="form-grid auth-fields">
                    <TextField label="Key filename" value={target.auth.path} onChange={(path) => updateAuth({ path })} />
                    <SecretField label="Passphrase" shown={showSecret} value={target.auth.passphrase ?? ""} onToggle={() => setShowSecret((value) => !value)} onChange={(passphrase) => updateAuth({ passphrase: passphrase || null })} />
                    <button className="secondary key-directory" onClick={() => invoke("open_keys")}>
                      <FolderKey size={16} /> Key directory
                    </button>
                  </div>
                )}
              </div>

              <div className="form-actions">
                <button
                  className="secondary"
                  disabled={busy || saving}
                  onClick={async () => { if (await save()) await onTest(target.id); }}
                >
                  <Activity size={16} /> {busy ? "Testing" : "Save and test"}
                </button>
              </div>
            </>
          ) : (
            <div className="target-placeholder"><TerminalSquare size={24} /><span>Add a target to begin</span></div>
          )}
        </div>
      </div>
    </section>
  );
}

function NumberField({ label, value, min, max, onChange }: { label: string; value: number; min?: number; max?: number; onChange: (value: number) => void }) {
  return <label className="field"><span>{label}</span><input type="number" min={min} max={max} value={value} onChange={(event) => onChange(Number(event.target.value))} /></label>;
}

function TextField({ label, value, onChange }: { label: string; value: string; onChange: (value: string) => void }) {
  return <label className="field"><span>{label}</span><input value={value} onChange={(event) => onChange(event.target.value)} /></label>;
}

function SecretField({ label, value, shown, onToggle, onChange }: { label: string; value: string; shown: boolean; onToggle: () => void; onChange: (value: string) => void }) {
  return <label className="field secret-field"><span>{label}</span><div><input type={shown ? "text" : "password"} value={value} onChange={(event) => onChange(event.target.value)} /><button type="button" className="icon" title={shown ? "Hide secret" : "Show secret"} onClick={onToggle}>{shown ? <EyeOff size={16} /> : <Eye size={16} />}</button></div></label>;
}

type TranscriptState = {
  commands: Map<string, SessionCommand>;
  activeCommandId: string | null;
  finishedCommandIds: Set<string>;
  previousExecByteWasCr: boolean;
  atLineStart: boolean;
  wroteOutput: boolean;
};

function compactCommand(value: string): string {
  const clean = value
    .replace(/[\u0000-\u001f\u007f-\u009f]+/g, " ")
    .replace(/\s+/g, " ")
    .trim();
  return clean.length <= 240 ? clean : `${clean.slice(0, 237)}...`;
}

function execPayloadWithTerminalEol(payload: number[], previousWasCr: boolean) {
  const output: number[] = [];
  let wasCr = previousWasCr;
  let atLineStart = false;
  for (const byte of payload) {
    if (byte === 0x0a && !wasCr) output.push(0x0d);
    output.push(byte);
    wasCr = byte === 0x0d;
    atLineStart = byte === 0x0a;
  }
  return { payload: new Uint8Array(output), previousWasCr: wasCr, atLineStart };
}

function finishTranscriptCommand(term: Terminal, state: TranscriptState, force = false) {
  const id = state.activeCommandId;
  if (!id || state.finishedCommandIds.has(id)) return;
  const command = state.commands.get(id);
  if (!command || command.status === "running") {
    if (!force) return;
    if (!state.atLineStart) term.write("\r\n");
  } else {
    const label = command.status === "completed"
      ? `exit ${command.exit_code ?? 0}`
      : command.exit_code == null
        ? command.status.replace("_", " ")
        : `exit ${command.exit_code}`;
    const color = command.status === "completed" ? "\x1b[90m" : "\x1b[31m";
    if (!state.atLineStart) term.write("\r\n");
    term.write(`${color}[${label}]\x1b[0m\r\n`);
  }
  state.finishedCommandIds.add(id);
  state.activeCommandId = null;
  state.previousExecByteWasCr = false;
  state.atLineStart = true;
  state.wroteOutput = true;
}

function beginTranscriptCommand(term: Terminal, state: TranscriptState, command: SessionCommand) {
  if (state.activeCommandId === command.id || state.finishedCommandIds.has(command.id)) return;
  if (state.activeCommandId) finishTranscriptCommand(term, state, true);
  const prefix = state.wroteOutput ? (state.atLineStart ? "\r\n" : "\r\n\r\n") : "";
  term.write(`${prefix}\x1b[1;32m$\x1b[0m ${compactCommand(command.command)}\r\n`);
  state.activeCommandId = command.id;
  state.previousExecByteWasCr = false;
  state.atLineStart = true;
  state.wroteOutput = true;
}

function appendTranscript(
  term: Terminal,
  state: TranscriptState,
  commands: SessionCommand[],
  events: TerminalEvent[],
  hasMore: boolean,
) {
  for (const command of commands) state.commands.set(command.id, command);

  for (const event of events) {
    if (event.command_id) {
      const command = state.commands.get(event.command_id);
      if (command) beginTranscriptCommand(term, state, command);
      const normalized = execPayloadWithTerminalEol(
        event.payload,
        state.previousExecByteWasCr,
      );
      term.write(normalized.payload);
      state.previousExecByteWasCr = normalized.previousWasCr;
      state.atLineStart = normalized.atLineStart;
      state.wroteOutput = true;
      continue;
    }

    if (state.activeCommandId) finishTranscriptCommand(term, state, true);
    term.write(new Uint8Array(event.payload));
    state.previousExecByteWasCr = false;
    state.atLineStart = event.payload.at(-1) === 0x0a;
    state.wroteOutput = true;
  }

  if (!events.length && commands.length) {
    const latest = commands.reduce((left, right) =>
      left.started_at > right.started_at ? left : right);
    beginTranscriptCommand(term, state, latest);
  }
  if (!hasMore) finishTranscriptCommand(term, state);
}

function SessionDetail({ id }: { id: string }) {
  const [session, setSession] = useState<Session | null>(null);
  const [error, setError] = useState("");
  const host = useRef<HTMLDivElement>(null);
  const terminal = useRef<Terminal | null>(null);
  const sequence = useRef(0);
  const transcript = useRef<TranscriptState>({
    commands: new Map(),
    activeCommandId: null,
    finishedCommandIds: new Set(),
    previousExecByteWasCr: false,
    atLineStart: true,
    wroteOutput: false,
  });

  useEffect(() => {
    if (!host.current) return;
    const term = new Terminal({
      disableStdin: true,
      convertEol: false,
      cursorBlink: false,
      fontFamily: "SFMono-Regular, Menlo, monospace",
      fontSize: 13,
      theme: {
        background: "#111416", foreground: "#e7ebed", cursor: "#111416",
        red: "#ff6b5f", green: "#80c995", yellow: "#e5c07b", blue: "#75a7ff",
        magenta: "#c792ea", cyan: "#64d8cb", white: "#e7ebed",
      },
      scrollback: 10000,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host.current);
    fit.fit();
    terminal.current = term;
    const resize = () => fit.fit();
    const observer = new ResizeObserver(resize);
    observer.observe(host.current);
    window.addEventListener("resize", resize);
    return () => {
      observer.disconnect();
      window.removeEventListener("resize", resize);
      term.dispose();
    };
  }, []);

  useEffect(() => {
    let active = true;
    const poll = async () => {
      try {
        const status = await invoke<ResponseData>("session_status", { sessionId: id });
        if (status.kind === "session") setSession(status.data);
        let more = true;
        while (active && more) {
          const output = await invoke<ResponseData>("session_events", { sessionId: id, afterSequence: sequence.current, maxBytes: 65536 });
          if (output.kind !== "events") break;
          if (terminal.current) {
            appendTranscript(
              terminal.current,
              transcript.current,
              output.data.commands ?? (output.data.command ? [output.data.command] : []),
              output.data.events,
              output.data.has_more,
            );
          }
          for (const event of output.data.events) {
            sequence.current = Math.max(sequence.current, event.sequence);
          }
          more = output.data.has_more;
        }
        setError("");
      } catch (cause) {
        setError(String(cause));
      }
    };
    void poll();
    const timer = window.setInterval(poll, 250);
    return () => { active = false; window.clearInterval(timer); };
  }, [id]);

  return (
    <section className="session-detail">
      <div className="session-detail-header">
        <div>
          <span>Session detail</span>
          <h2>{session?.target_name || "Loading session"}</h2>
        </div>
        {session && <Status value={session.status} />}
      </div>
      <section className="session-meta">
        <div><span className="label">Session</span><code>{id}</code></div>
        <div><span className="label">AI client</span><span>{session?.client_name || "-"}</span></div>
        <div><span className="label">Exit</span><span>{session?.last_exit_code == null ? "Pending" : session.last_exit_code}</span></div>
      </section>
      <div className="warning"><ShieldAlert size={16} /><span>{session?.recording_truncated ? "Recording limit reached; live output continues" : "Host verification is disabled"}</span><code>{session?.host_fingerprint || "Fingerprint pending"}</code></div>
      {error && <div className="error-banner">{error}</div>}
      <section className="terminal-shell">
        <div className="terminal-bar"><span><TerminalSquare size={16} />Read-only terminal</span><code>{session?.current_command || "No foreground command"}</code></div>
        <div className="terminal" ref={host} />
      </section>
    </section>
  );
}

function Header({ children }: { children?: React.ReactNode }) {
  return <header><div className="brand"><div className="brand-mark">AI</div><div><h1>AI SSH</h1><span>Local session manager</span></div></div>{children}</header>;
}

function Status({ value }: { value: SessionStatus }) {
  return <span className={`status status-${value}`}><i />{value.replace("_", " ")}</span>;
}

createRoot(document.getElementById("root")!).render(<App />);
