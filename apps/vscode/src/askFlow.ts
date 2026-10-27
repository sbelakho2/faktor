// The plain conversational (Ask) path.
//
// Ask is NOT a task run. It posts ONE ordinary prompt through the daemon's
// `POST /native/session/{id}/prompt` route and reads only conversation data
// back. It never constructs an EngineeringContract, never starts a task run,
// never reads proof/verification, and never touches billing/control-plane
// routes. The extension host and the live tests share this module so the
// isolation is executable, not documentation.

import type {
  NativeClient,
  NativeMessagePage,
  NativePromptReceipt,
  NativeSessionSummary,
} from './nativeClient.ts';

export interface AskPromptInput {
  readonly sessionId: string;
  readonly prompt: string;
  readonly files: readonly string[];
  readonly submissionId: string;
}

export interface AskPromptClient {
  prompt(
    sessionId: string,
    prompt: string,
    options: { readonly files?: readonly string[]; readonly submissionId: string },
  ): Promise<NativePromptReceipt>;
}

/** Post one plain prompt. No task, contract, proof or billing member. */
export async function sendAskTurn(
  client: AskPromptClient,
  input: AskPromptInput,
): Promise<NativePromptReceipt> {
  return client.prompt(input.sessionId, input.prompt, {
    files: input.files.length > 0 ? input.files : undefined,
    submissionId: input.submissionId,
  });
}

export interface ConversationClient {
  messages(
    sessionId: string,
    page?: { readonly before?: number; readonly limit?: number },
  ): Promise<NativeMessagePage>;
  listSessions(): Promise<NativeSessionSummary[]>;
  permissions(sessionId: string): Promise<readonly unknown[]>;
}

export interface ConversationRead {
  readonly messages: NativeMessagePage;
  readonly sessions: readonly NativeSessionSummary[];
  readonly permissions: readonly unknown[] | null;
}

/**
 * The ONLY reads an Ask turn performs. Deliberately no `tasks`, `taskRuns`,
 * `verification`, `proof`, `tournament`, board, entitlement or billing call.
 */
export async function loadConversation(
  client: ConversationClient,
  sessionId: string,
  limit: number,
): Promise<ConversationRead> {
  const [messages, listing, permissions] = await Promise.all([
    client.messages(sessionId, { limit }),
    client.listSessions(),
    client.permissions(sessionId).then(
      (rows) => rows,
      () => null,
    ),
  ]);
  return { messages, sessions: listing, permissions };
}

/** TRUE only for the native client the extension actually uses. */
export function isNativeConversationClient(client: NativeClient): client is NativeClient {
  return typeof client.messages === 'function' && typeof client.listSessions === 'function';
}
