import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type { RefObject } from "react";
import { CreateSession, initialCreateDraft } from "./create-session";
import type { ReadinessStatus } from "./create-session";
import { localClient, mergeCreateOperations, workspaceIssueText } from "./local-client";
import type { CreateDisposition, CreateIntent, LocalCreate, ProviderProfile } from "./local-client";

const profiles = [
  { value: "claude_code", label: "Claude Code" },
  { value: "codex", label: "Codex" },
] as const;

interface CreateSessionDialogProps {
  readonly onDismiss: () => void;
  readonly onOpen: (operation: LocalCreate) => Promise<boolean>;
  readonly captureSubmission: () => (operation: LocalCreate) => boolean;
  readonly returnFocus: RefObject<HTMLButtonElement | null>;
  readonly connection: number;
}

function statusText(disposition: CreateDisposition): string {
  switch (disposition.result) {
    case "pending": return "Спробу збережено. Можна продовжити з тим самим запитом.";
    case "failed": return workspaceIssueText[disposition.issue];
    case "unknown": return `Результат запуску невідомий. Оновіть стан цієї спроби.${disposition.issue ? ` ${workspaceIssueText[disposition.issue]}` : ""}`;
    case "launched": return disposition.current === "live"
      ? "Термінал запущено. Стан входу та довіри перевірте в CLI."
      : "Запуск підтверджено. Поточний термінал недоступний.";
    case "exited": return disposition.setup_issue
      ? `Помилка підготовки CLI. ${workspaceIssueText[disposition.setup_issue]}`
      : "Процес у терміналі завершився.";
  }
}

export function CreateSessionDialog({ onDismiss, onOpen, captureSubmission, returnFocus, connection }: CreateSessionDialogProps) {
  const dialogRef = useRef<HTMLDialogElement>(null);
  const mounted = useRef(false);
  const completed = useRef(false);
  const inAction = useRef(false);
  const refreshVersion = useRef(0);
  const [operations, setOperations] = useState<readonly LocalCreate[]>([]);
  const [pending, setPending] = useState<"picker" | "submit" | "status" | null>(null);
  const [readinessStatus, setReadinessStatus] = useState<ReadinessStatus>(null);
  const [draft, setDraft] = useState(initialCreateDraft);
  const [creation] = useState(() => new CreateSession(setDraft, setReadinessStatus));
  const { workspace, profile } = draft;
  const [feedback, setFeedback] = useState("");
  const [storeIssue, setStoreIssue] = useState("");
  const [confirmSeparate, setConfirmSeparate] = useState<string | null>(null);

  useEffect(() => {
    const dialog = dialogRef.current;
    const trigger = returnFocus.current;
    mounted.current = true;
    dialog?.showModal();
    return () => {
      mounted.current = false;
      dialog?.close();
      if (!completed.current) trigger?.focus();
    };
  }, [returnFocus]);

  const workspaceId = workspace?.workspace_id;
  useLayoutEffect(() => {
    void creation.check(draft);
    return () => creation.invalidate();
  }, [creation, draft, connection]);

  useEffect(() => {
    let disposed = false;
    const version = ++refreshVersion.current;
    void localClient.createStatus().then(status => {
      if (disposed || version !== refreshVersion.current) return;
      setOperations(current => mergeCreateOperations(current, status.operations));
      setStoreIssue(status.issue ? workspaceIssueText[status.issue] : "");
    }).catch(() => {
      if (!disposed && version === refreshVersion.current) setStoreIssue("Не вдалося відновити записи створення.");
    });
    return () => { disposed = true; };
  }, [connection]);

  async function act(kind: NonNullable<typeof pending>, action: () => Promise<void>) {
    if (inAction.current) return;
    inAction.current = true;
    if (kind !== "picker") {
      refreshVersion.current += 1;
      setFeedback("");
    }
    setPending(kind);
    try {
      await action();
    } catch {
      if (mounted.current) setFeedback("Не вдалося отримати відповідь. Оновіть записи створення після підключення.");
    } finally {
      inAction.current = false;
      if (mounted.current) setPending(null);
    }
  }

  function chooseWorkspace() {
    return act("picker", async () => {
      const choice = await creation.chooseWorkspace(() => mounted.current && dialogRef.current !== null);
      if (!choice || !mounted.current) return;
      setFeedback(choice.result === "blocked" ? workspaceIssueText[choice.issue] : "");
    });
  }

  function dismiss() {
    creation.invalidate();
    mounted.current = false;
    onDismiss();
  }

  function selectProfile(next: ProviderProfile) {
    creation.select(workspace, next);
    setFeedback("");
  }

  function submit(id: string, selectedProfile: ProviderProfile, intent?: CreateIntent) {
    if (!mounted.current || inAction.current) return;
    return creation.submit(id, selectedProfile, intent, send => {
      const present = captureSubmission();
      return act("submit", async () => {
        const reply = await send();
        if (mounted.current) setConfirmSeparate(null);
        if (reply.result === "blocked") {
          if (mounted.current) setFeedback(workspaceIssueText[reply.issue]);
          return;
        }
        if (mounted.current) setOperations(current => mergeCreateOperations(current, [reply.operation]));
        const disposition = reply.operation.disposition;
        if (disposition.result === "launched" && disposition.current === "live") {
          completed.current = present(reply.operation);
          if (!completed.current && mounted.current) setFeedback("Відкрийте цей термінал із запису попередньої спроби.");
        }
      });
    });
  }

  function refresh(reference?: string) {
    return act("status", async () => {
      const status = await localClient.createStatus(reference);
      if (!mounted.current) return;
      setOperations(current => mergeCreateOperations(current, status.operations));
      if (!reference || status.issue) setStoreIssue(status.issue ? workspaceIssueText[status.issue] : "");
    });
  }

  const ready = workspaceId !== undefined && creation.allows(workspaceId, profile);
  const readinessText = readinessStatus === "pending" ? "Перевіряємо папку та CLI в агенті Mac…" :
    readinessStatus === "unavailable" ? "Не вдалося перевірити папку та CLI. Повторіть перевірку." :
    readinessStatus?.result === "blocked" ? workspaceIssueText[readinessStatus.issue] : "";

  return <dialog
    ref={dialogRef}
    aria-labelledby="create-session-title"
    onCancel={event => { event.preventDefault(); dismiss(); }}
    style={{ color: "inherit", background: "var(--surface)", border: "1px solid var(--border)", borderRadius: 12,
      padding: 24, maxWidth: "min(34rem, calc(100vw - 32px))" }}
  >
    <h2 id="create-session-title" style={{ marginTop: 0 }}>Створити локальну сесію</h2>
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
          onChange={() => selectProfile(choice.value)} /> {choice.label}
      </label>)}
    </fieldset>
    <p role="status" aria-live="polite">
      {pending === "picker" ? "Очікуємо вибору папки…" : pending === "submit" ? "Створюємо термінал…" :
        pending === "status" ? "Оновлюємо стан…" : feedback}
    </p>
    {readinessText && <p role="status">{readinessText}</p>}
    {workspace && pending === null && !ready && readinessStatus !== "pending" &&
      <button type="button" onClick={() => creation.retry()}>Перевірити знову</button>}
    {(operations.length > 0 || storeIssue) && <section aria-labelledby="create-recovery-title" style={{ maxHeight: "40vh", overflow: "auto" }}>
      <h3 id="create-recovery-title">Попередні спроби</h3>
      {storeIssue && <p role="status">{storeIssue}</p>}
      <button type="button" disabled={pending !== null} onClick={() => void refresh()}>Оновити всі</button>
      <ul>
        {operations.map(operation => {
          const disposition = operation.disposition;
          const label = profiles.find(choice => choice.value === operation.profile)?.label;
          const name = workspace?.workspace_id === operation.workspace_id ? workspace.label : label;
          const id = operation.reference.slice(0, 8);
          return <li key={operation.reference}>
            <p><strong>{name} · {id}</strong> <span role="status">{statusText(disposition)}</span></p>
            <div style={{ display: "flex", flexWrap: "wrap", gap: 8 }}>
              <button type="button" disabled={pending !== null} aria-label={`Оновити спробу ${id}`} onClick={() => void refresh(operation.reference)}>Оновити</button>
              {((disposition.result === "launched" && disposition.current === "live") ||
                (disposition.result === "exited" && disposition.current === "exited")) &&
                <button type="button" disabled={pending !== null} onClick={() => void act("status", async () => {
                  completed.current = await onOpen(operation);
                  if (!completed.current && mounted.current) setFeedback("Не вдалося відкрити вкладку. Оновіть запис і спробуйте знову.");
                })}>Відкрити вкладку</button>}
              {disposition.result === "pending" &&
                <button type="button" disabled={pending !== null} onClick={() => void submit(operation.workspace_id, operation.profile, { continueReference: operation.reference })}>Продовжити</button>}
              {disposition.result !== "pending" && confirmSeparate !== operation.reference &&
                <button type="button" disabled={pending !== null} onClick={() => {
                  creation.select(workspaceId === operation.workspace_id ? workspace :
                    { workspace_id: operation.workspace_id, label: name ?? id }, operation.profile);
                  setFeedback("");
                  setConfirmSeparate(operation.reference);
                }}>Створити окрему сесію…</button>}
            </div>
            {confirmSeparate === operation.reference && <div>
              <p>{disposition.result === "unknown"
                ? "Результат попередньої спроби залишається невідомим. Окрема сесія може запустити ще один процес."
                : "Буде створено окремий процес із цією папкою та профілем."}</p>
              <button type="button" onClick={() => setConfirmSeparate(null)}>Назад</button>{" "}
              <button type="button" disabled={pending !== null || !ready || workspaceId !== operation.workspace_id || profile !== operation.profile}
                onClick={() => void submit(operation.workspace_id, operation.profile, { separateFrom: operation.reference })}>Створити окремо</button>
            </div>}
          </li>;
        })}
      </ul>
    </section>}
    <div style={{ display: "flex", flexWrap: "wrap", justifyContent: "end", gap: 8 }}>
      <button type="button" onClick={dismiss}>{pending === "submit" || operations.length > 0 ? "Закрити" : "Скасувати"}</button>
      <button type="button" disabled={!ready || pending !== null || confirmSeparate !== null}
        onClick={() => { if (workspaceId && confirmSeparate === null) void submit(workspaceId, profile); }}>
        Створити сесію
      </button>
    </div>
  </dialog>;
}
