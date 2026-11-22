import { useEffect, useRef, useState } from "react";
import type { RefObject } from "react";
import { localClient, workspaceIssueText } from "./local-client";
import type { ProviderProfile, WorkspaceDisplay } from "./local-client";

const profiles = [
  { value: "claude_code", label: "Claude Code" },
  { value: "codex", label: "Codex" },
] as const;

interface CreateSessionDialogProps {
  readonly onDismiss: () => void;
  readonly returnFocus: RefObject<HTMLButtonElement | null>;
}

export function CreateSessionDialog({ onDismiss, returnFocus }: CreateSessionDialogProps) {
  const dialogRef = useRef<HTMLDialogElement>(null);
  const [workspace, setWorkspace] = useState<WorkspaceDisplay | null>(null);
  const [profile, setProfile] = useState<ProviderProfile>("claude_code");
  const [pending, setPending] = useState<"picker" | "readiness" | null>(null);
  const [feedback, setFeedback] = useState("");

  useEffect(() => {
    const dialog = dialogRef.current;
    const trigger = returnFocus.current;
    dialog?.showModal();
    return () => {
      dialog?.close();
      trigger?.focus();
    };
  }, [returnFocus]);

  async function chooseWorkspace() {
    setPending("picker");
    try {
      const choice = await localClient.pickWorkspace();
      if (choice.result === "selected") {
        setWorkspace(choice.workspace);
        setFeedback("");
      } else if (choice.result === "blocked") {
        setFeedback(workspaceIssueText[choice.issue]);
      }
    } catch {
      setFeedback("Не вдалося вибрати папку. Перевірте підключення до агента Mac і спробуйте ще раз.");
    } finally {
      setPending(null);
    }
  }

  async function checkReadiness() {
    if (!workspace) return;
    setPending("readiness");
    setFeedback("");
    try {
      const readiness = await localClient.workspaceReadiness(workspace.workspace_id, profile);
      setFeedback(readiness.result === "ready"
        ? "Папка доступна. Агент Mac знайшов вибраний CLI."
        : workspaceIssueText[readiness.issue]);
    } catch {
      setFeedback("Не вдалося перевірити профіль. Перевірте підключення до агента Mac і спробуйте ще раз.");
    } finally {
      setPending(null);
    }
  }

  return <dialog
    ref={dialogRef}
    aria-labelledby="workspace-preflight-title"
    onCancel={event => { event.preventDefault(); onDismiss(); }}
    style={{ color: "inherit", background: "var(--surface)", border: "1px solid var(--border)", borderRadius: 12,
      padding: 24, maxWidth: "min(30rem, calc(100vw - 32px))" }}
  >
    <h2 id="workspace-preflight-title" style={{ marginTop: 0 }}>Перевірка проєкту</h2>
    <section aria-label="Папка проєкту" style={{ overflowWrap: "anywhere" }}>
      {workspace && <p><strong>{workspace.label}</strong></p>}
      <button type="button" disabled={pending !== null} onClick={() => void chooseWorkspace()}>
        {workspace ? "Змінити папку…" : "Вибрати папку…"}
      </button>
    </section>
    <fieldset disabled={pending !== null} style={{ border: "1px solid var(--border)", margin: "16px 0 0" }}>
      <legend>Профіль</legend>
      {profiles.map(choice => <label key={choice.value} style={{ display: "block", padding: "4px 0" }}>
        <input type="radio" name="provider-profile" value={choice.value} checked={profile === choice.value}
          onChange={() => { setProfile(choice.value); setFeedback(""); }} /> {choice.label}
      </label>)}
    </fieldset>
    <p role="status" aria-live="polite">
      {pending === "picker" ? "Очікуємо вибору папки…" : pending === "readiness" ? "Перевіряємо доступ із агента Mac…" : feedback}
    </p>
    <div style={{ display: "flex", flexWrap: "wrap", justifyContent: "end", gap: 8 }}>
      <button type="button" onClick={onDismiss}>Скасувати</button>
      <button type="button" disabled={!workspace || pending !== null} onClick={() => void checkReadiness()}>
        Перевірити готовність
      </button>
    </div>
  </dialog>;
}
