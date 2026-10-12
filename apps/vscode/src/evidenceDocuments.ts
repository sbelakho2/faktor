// The ONE VS Code carrier of the immutable evidence documents (I03): a
// TextDocumentContentProvider for the dedicated `faktor-evidence` scheme.
//
// The provider renders ONLY served evidence bytes through
// `loadEvidenceDocument` (see changeReview.ts). It never reads a workspace
// file, never writes, and a typed `ChangeReviewError` renders an explicit
// refusal document (the code is kept verbatim) rather than fabricating
// content. The untrusted-message boundary is the strict URI parser; the
// client is resolved lazily so a document request after a daemon stop
// refuses typed instead of touching anything.

import * as vscode from 'vscode';
import {
  ChangeReviewError,
  DIFF_UNAVAILABLE_LABEL,
  FAKTOR_EVIDENCE_SCHEME,
  loadEvidenceDocument,
  parseEvidenceDocumentUri,
  type EvidenceReader,
} from './changeReview.ts';

export function registerEvidenceDocumentProvider(
  client: () => EvidenceReader | null,
): vscode.Disposable {
  return vscode.workspace.registerTextDocumentContentProvider(FAKTOR_EVIDENCE_SCHEME, {
    provideTextDocumentContent: async (uri) => {
      try {
        const request = parseEvidenceDocumentUri({ scheme: uri.scheme, path: uri.path });
        const reader = client();
        if (reader === null) {
          throw new ChangeReviewError('fetch_failed', 'the Faktor service is not running');
        }
        return await loadEvidenceDocument(request, reader);
      } catch (error) {
        if (error instanceof ChangeReviewError) {
          return `[Faktor] ${DIFF_UNAVAILABLE_LABEL} — ${error.code}: ${error.message}\n`;
        }
        return `[Faktor] ${DIFF_UNAVAILABLE_LABEL} — the immutable evidence could not be read.\n`;
      }
    },
  });
}
