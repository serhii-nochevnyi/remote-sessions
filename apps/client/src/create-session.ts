import { CreateReadiness, localClient } from "./local-client.ts";
import type { CreateIntent, ProviderProfile, WorkspaceDisplay, WorkspaceReadiness } from "./local-client.ts";

export interface CreateDraft {
  readonly workspace: WorkspaceDisplay | null;
  readonly profile: ProviderProfile;
}

export const initialCreateDraft: CreateDraft = { workspace: null, profile: "claude_code" };

export type ReadinessStatus = WorkspaceReadiness | "pending" | "unavailable" | null;

export class CreateSession {
  private readonly readiness = new CreateReadiness();
  private readonly publishDraft: (draft: CreateDraft) => void;
  private readonly publishReadiness: (status: ReadinessStatus) => void;
  private currentDraft = initialCreateDraft;

  constructor(
    publishDraft: (draft: CreateDraft) => void,
    publishReadiness: (status: ReadinessStatus) => void,
  ) {
    this.publishDraft = publishDraft;
    this.publishReadiness = publishReadiness;
  }

  get draft() {
    return this.currentDraft;
  }

  private replaceDraft(workspace: WorkspaceDisplay | null, profile: ProviderProfile) {
    // Each replacement is an effect key, even when the selected pair is identical.
    this.invalidate();
    this.currentDraft = { workspace, profile };
    this.publishDraft(this.currentDraft);
  }

  select(workspace: WorkspaceDisplay | null, profile: ProviderProfile) {
    if (workspace?.workspace_id === this.draft.workspace?.workspace_id && profile === this.draft.profile) return;
    this.replaceDraft(workspace, profile);
  }

  retry() {
    this.replaceDraft(this.draft.workspace, this.draft.profile);
  }

  async chooseWorkspace(isMounted: () => boolean) {
    const choice = await localClient.pickWorkspace();
    if (!isMounted() || choice.result === "cancelled") return;
    this.replaceDraft(choice.result === "selected" ? choice.workspace : null, this.draft.profile);
    return choice;
  }

  async check(draft: CreateDraft) {
    if (draft !== this.draft) return;
    const { workspace, profile } = draft;
    if (!workspace) {
      this.publishReadiness(null);
      return;
    }
    const check = this.readiness.begin(workspace.workspace_id, profile);
    this.publishReadiness("pending");
    const result = await localClient.workspaceReadiness(workspace.workspace_id, profile).catch(() => undefined);
    if (this.readiness.finish(check, result)) this.publishReadiness(result ?? "unavailable");
  }

  invalidate() {
    this.readiness.invalidate();
  }

  allows(workspaceId: string, profile: ProviderProfile) {
    return this.readiness.allows(workspaceId, profile);
  }

  submit(
    workspaceId: string,
    profile: ProviderProfile,
    intent: CreateIntent | undefined,
    dispatch: (send: () => ReturnType<typeof localClient.createSession>) => Promise<void>,
  ) {
    if (!intent?.continueReference) {
      if (!this.readiness.take(workspaceId, profile)) return;
      this.publishReadiness(null);
    }
    return dispatch(() => localClient.createSession(workspaceId, profile, intent));
  }
}
