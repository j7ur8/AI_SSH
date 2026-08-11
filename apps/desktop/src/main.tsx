import React, { useEffect, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { invoke } from "@tauri-apps/api/core";
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
  sequence: number;
  stream: "stdout" | "stderr" | "pty" | "system";
  payload: number[];
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
        events: TerminalEvent[];
        next_sequence: number;
        has_more: boolean;
      };
    };

const sessionId = new URLSearchParams(location.search).get("session");

function App() {
  return sessionId ? <SessionWindow id={sessionId} /> : <Dashboard />;
}

function Dashboard() {
  const [view, setView] = useState<"sessions" | "configuration">("sessions");
  const [sessions, setSessions] = useState<Session[]>([]);
  const [config, setConfig] = useState<AppConfig | null>(null);
  const [error, setError] = useState("");
  const [connectionError, setConnectionError] = useState("");
  const [busy, setBusy] = useState(false);

  const loadSessions = async () => {
    try {
      const value = await invoke<ResponseData>("sessions", { includeHistory: true });
      if (value.kind === "sessions") {
        setSessions(value.data);
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
    return () => window.clearInterval(timer);
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
        <SessionsView sessions={sessions} busy={busy} onRefresh={loadSessions} />
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
  busy,
  onRefresh,
}: {
  sessions: Session[];
  busy: boolean;
  onRefresh: () => Promise<void>;
}) {
  return (
    <section className="content">
      <div className="section-title">
        <div>
          <h2>Sessions</h2>
          <span>{sessions.length} recorded</span>
        </div>
        <button className="icon" title="Refresh sessions" onClick={() => void onRefresh()}>
          <RefreshCw size={17} />
        </button>
      </div>
      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Target</th>
              <th>Status</th>
              <th>Current command</th>
              <th>AI client</th>
              <th>Started</th>
              <th aria-label="Actions" />
            </tr>
          </thead>
          <tbody>
            {sessions.map((item) => (
              <tr key={item.id}>
                <td>
                  <strong>{item.target_name}</strong>
                  <small>{item.id.slice(0, 8)}</small>
                </td>
                <td><Status value={item.status} /></td>
                <td className="command">{item.current_command || "-"}</td>
                <td>{item.client_name}</td>
                <td>{new Date(item.created_at).toLocaleString()}</td>
                <td className="row-action">
                  <button
                    className="icon"
                    title="Open read-only terminal"
                    onClick={() => invoke("show_session", { sessionId: item.id })}
                  >
                    <Eye size={16} />
                  </button>
                </td>
              </tr>
            ))}
            {!sessions.length && !busy && (
              <tr><td colSpan={6} className="empty">No sessions recorded</td></tr>
            )}
          </tbody>
        </table>
      </div>
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

function SessionWindow({ id }: { id: string }) {
  const [session, setSession] = useState<Session | null>(null);
  const [error, setError] = useState("");
  const host = useRef<HTMLDivElement>(null);
  const terminal = useRef<Terminal | null>(null);
  const sequence = useRef(0);

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
    window.addEventListener("resize", resize);
    return () => { window.removeEventListener("resize", resize); term.dispose(); };
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
          for (const event of output.data.events) {
            terminal.current?.write(new Uint8Array(event.payload));
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
    <main className="session-page">
      <Header />
      <section className="session-meta">
        <div><span className="label">Target</span><strong>{session?.target_name || "Connecting"}</strong></div>
        <div><span className="label">Session</span><code>{id}</code></div>
        <div><span className="label">AI client</span><span>{session?.client_name || "-"}</span></div>
        <div><span className="label">Status / exit</span>{session && <Status value={session.status} />}<small>{session?.last_exit_code == null ? "Exit pending" : `Exit ${session.last_exit_code}`}</small></div>
      </section>
      <div className="warning"><ShieldAlert size={16} /><span>{session?.recording_truncated ? "Recording limit reached; live output continues" : "Host verification is disabled"}</span><code>{session?.host_fingerprint || "Fingerprint pending"}</code></div>
      {error && <div className="error-banner">{error}</div>}
      <section className="terminal-shell">
        <div className="terminal-bar"><span><TerminalSquare size={16} />Read-only terminal</span><code>{session?.current_command || "No foreground command"}</code></div>
        <div className="terminal" ref={host} />
      </section>
    </main>
  );
}

function Header({ children }: { children?: React.ReactNode }) {
  return <header><div className="brand"><div className="brand-mark">AI</div><div><h1>AI SSH</h1><span>Local session manager</span></div></div>{children}</header>;
}

function Status({ value }: { value: SessionStatus }) {
  return <span className={`status status-${value}`}><i />{value.replace("_", " ")}</span>;
}

createRoot(document.getElementById("root")!).render(<React.StrictMode><App /></React.StrictMode>);
