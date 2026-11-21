export interface DemoSession {
  readonly id: string;
  readonly name: string;
  readonly agent: "Claude Code" | "Codex" | "Shell";
  readonly project: string;
  readonly output: string;
}

export const demoSessions: readonly DemoSession[] = [
  { id: "claude-auth", name: "Авторизація", agent: "Claude Code", project: "api-service", output: "[Демонстраційні дані · CLI не запущено]\n\n❯ claude\n\n❯ Додай тести авторизації користувачів\n\nRead  src/auth/session.ts\nRead  tests/auth.test.ts\n\nТри сценарії для перевірки:\n  • чинний токен\n  • прострочений токен\n  • відсутній токен\n\nЦе приклад виведення для перевірки рендерера." },
  { id: "codex-tests", name: "Тести входу", agent: "Codex", project: "web-app", output: "[Демонстраційні дані · CLI не запущено]\n\n❯ codex\n\n❯ Додай тести форми входу\n\nRead  src/LoginForm.tsx\nWrite tests/login.test.ts\n\n✓ порожні поля\n✓ помилковий пароль\n✓ успішний вхід\n\nРезультати тестів тут ілюстративні." },
  { id: "shell-server", name: "Dev server", agent: "Shell", project: "api-service", output: "[Демонстраційні дані · shell не запущено]\n\n❯ npm run dev\n\n> api-service dev\n> tsx watch src/server.ts\n\nServer listening on localhost:3000\n\nGET /health       200\nPOST /auth/login  200\n\nЦе приклад журналу; сервер не запускався." },
];

export interface SessionViews {
  readonly open: readonly string[];
  readonly active: string | null;
}

export function openView(state: SessionViews, id: string, knownIds: readonly string[] = demoSessions.map(session => session.id)): SessionViews {
  if (!knownIds.includes(id)) return state;
  return { open: state.open.includes(id) ? state.open : [...state.open, id], active: id };
}

// Views contain no runtime records, so closing one cannot delete a session.
export function closeView(state: SessionViews, id: string): SessionViews {
  const open = state.open.filter(item => item !== id);
  return { open, active: state.active === id ? open[0] ?? null : state.active };
}

export interface ScaffoldStatus {
  readonly protocolVersion: 1;
  readonly appVersion: string;
  readonly stage: "scaffold";
  readonly capabilities: readonly [];
}

export function parseScaffoldStatus(value: unknown): ScaffoldStatus {
  if (typeof value !== "object" || value === null ||
      !("protocolVersion" in value) || value.protocolVersion !== 1 ||
      !("appVersion" in value) || typeof value.appVersion !== "string" || value.appVersion.length === 0 ||
      !("stage" in value) || value.stage !== "scaffold" ||
      !("capabilities" in value) || !Array.isArray(value.capabilities) || value.capabilities.length !== 0 ||
      Object.keys(value).length !== 4) {
    throw new Error("Unsupported scaffold status");
  }
  return { protocolVersion: 1, appVersion: value.appVersion, stage: "scaffold", capabilities: [] };
}
