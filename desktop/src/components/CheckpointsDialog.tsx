import { Check, GitCompareArrows, History, RefreshCw, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { commands, type PreparedCheckpointReviewDto } from "../bindings";
import { getMessages } from "../lib/i18n";
import { useAppStore } from "../store/useAppStore";
import { SquaresLoader } from "./SquaresLoader";
import { Button } from "./ui/Button";
import { DialogShell } from "./ui/DialogShell";

export interface CheckpointsDialogProps {
  open: boolean;
  embedded?: boolean;
  sessionId: string | null;
  runActive: boolean;
  onClose: () => void;
}

interface CheckpointSummary {
  token: { id: string; sha256: string };
  parent: { id: string; sha256: string } | null;
  workspace_kind: string;
  source: { session_id: string; revision: number; label: string };
  files: number;
  bytes: number;
}

interface CheckpointListData {
  session_id: string;
  workspace: { path: string };
  page: number;
  total: number;
  next_page: number | null;
  checkpoints: CheckpointSummary[];
}

interface DiffFile {
  path: string;
  change: string;
  unified_diff: string | null;
  preview_truncated: boolean;
}

interface CheckpointDiffData {
  checkpoint: { id: string; sha256: string };
  preview_truncated: boolean;
  files: DiffFile[];
}

interface ReviewChange {
  path: string;
  desired?: { kind?: string };
}

interface RecoverFileOutcome {
  path: string;
  status: string;
}

interface RecoverOutcome {
  operation_id: string;
  checkpoint: { id: string; sha256: string };
  status: string;
  files: RecoverFileOutcome[];
  error: { code?: string; message?: string } | null;
}

interface CheckpointRecoverData {
  session_id: string;
  recovered: number;
  outcomes: RecoverOutcome[];
}

function formatBytes(bytes: number): string {
  if (bytes >= 1_048_576) return `${(bytes / 1_048_576).toFixed(1)} MB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${bytes} B`;
}

export function CheckpointsDialog({
  open,
  embedded = false,
  sessionId,
  runActive,
  onClose,
}: CheckpointsDialogProps) {
  const locale = useAppStore((s) => s.locale);
  const t = getMessages(locale);
  const d = t.checkpointsDialog;

  const [listData, setListData] = useState<CheckpointListData | null>(null);
  const [page, setPage] = useState(1);
  const [diffData, setDiffData] = useState<CheckpointDiffData | null>(null);
  const [diffFor, setDiffFor] = useState<string | null>(null);
  const [createPaths, setCreatePaths] = useState("");
  const [prepared, setPrepared] = useState<PreparedCheckpointReviewDto | null>(
    null
  );
  const [approvedPaths, setApprovedPaths] = useState<Set<string>>(new Set());
  const [confirmMsg, setConfirmMsg] = useState<string | null>(null);
  const [recoverData, setRecoverData] = useState<CheckpointRecoverData | null>(
    null
  );
  const [errorMsg, setErrorMsg] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const loadList = useCallback(
    async (targetPage: number) => {
      setErrorMsg(null);
      const res = await commands.checkpointList(
        sessionId ?? null,
        targetPage
      );
      if (res.status === "ok") {
        setListData(res.data.data as unknown as CheckpointListData);
        setPage(targetPage);
      } else {
        setErrorMsg(`${res.error.code}: ${res.error.message}`);
      }
    },
    [sessionId]
  );

  useEffect(() => {
    if (!open) return;
    setPrepared(null);
    setConfirmMsg(null);
    setDiffData(null);
    setDiffFor(null);
    setRecoverData(null);
    void loadList(1);
  }, [open, loadList]);

  const reviewChanges = useMemo<ReviewChange[]>(() => {
    if (!prepared) return [];
    const review = prepared.review as { kind?: string; changes?: ReviewChange[] };
    if (review.kind === "restore" && Array.isArray(review.changes)) {
      return review.changes;
    }
    return [];
  }, [prepared]);

  const isRestoreReview = reviewChanges.length > 0;

  const loadDiff = useCallback(
    async (summary: CheckpointSummary) => {
      setErrorMsg(null);
      setBusy(true);
      try {
        const res = await commands.checkpointDiff(
          sessionId ?? null,
          summary.token.id,
          summary.token.sha256
        );
        if (res.status === "ok") {
          setDiffData(res.data.data as unknown as CheckpointDiffData);
          setDiffFor(summary.token.id);
        } else {
          setErrorMsg(`${res.error.code}: ${res.error.message}`);
        }
      } finally {
        setBusy(false);
      }
    },
    [sessionId]
  );

  const handlePrepare = useCallback(
    async (action: "create" | "accept" | "restore", summary?: CheckpointSummary) => {
      setErrorMsg(null);
      setConfirmMsg(null);
      setBusy(true);
      try {
        const paths = createPaths
          .split(/[,\n]/)
          .map((p) => p.trim())
          .filter((p) => p.length > 0);
        const res = await commands.prepareCheckpointAction(
          sessionId ?? null,
          action,
          paths,
          summary ? summary.token.id : null,
          summary ? summary.token.sha256 : null,
          "desktop"
        );
        if (res.status === "ok") {
          setPrepared(res.data);
          const review = res.data.review as { kind?: string; changes?: ReviewChange[] };
          if (review.kind === "restore" && Array.isArray(review.changes)) {
            setApprovedPaths(new Set(review.changes.map((c) => c.path)));
          } else {
            setApprovedPaths(new Set());
          }
        } else {
          setErrorMsg(`${res.error.code}: ${res.error.message}`);
        }
      } finally {
        setBusy(false);
      }
    },
    [createPaths, sessionId]
  );

  const handleConfirm = useCallback(async () => {
    if (!prepared) return;
    setErrorMsg(null);
    setBusy(true);
    try {
      const res = await commands.confirmCheckpointAction(
        prepared.action_id,
        isRestoreReview ? Array.from(approvedPaths) : null
      );
      if (res.status === "ok") {
        setConfirmMsg(d.appliedMsg(prepared.operation_id));
        setPrepared(null);
        setApprovedPaths(new Set());
        setDiffData(null);
        setDiffFor(null);
        await loadList(page);
      } else {
        setErrorMsg(`${res.error.code}: ${res.error.message}`);
      }
    } finally {
      setBusy(false);
    }
  }, [prepared, isRestoreReview, approvedPaths, d, loadList, page]);

  const handleRecover = useCallback(async () => {
    setErrorMsg(null);
    setBusy(true);
    try {
      const res = await commands.checkpointRecover(sessionId ?? null);
      if (res.status === "ok") {
        setRecoverData(res.data.data as unknown as CheckpointRecoverData);
      } else {
        setErrorMsg(`${res.error.code}: ${res.error.message}`);
      }
    } finally {
      setBusy(false);
    }
  }, [sessionId]);

  const toggleApproved = useCallback((path: string) => {
    setApprovedPaths((prev) => {
      const next = new Set(prev);
      if (next.has(path)) {
        next.delete(path);
      } else {
        next.add(path);
      }
      return next;
    });
  }, []);

  if (!open) {
    return null;
  }

  const body = (
    <div className="space-y-4 text-xs">
      {errorMsg && (
        <div className="rounded-[var(--radius-md)] border border-[var(--danger)] bg-[var(--danger-surface)] px-3 py-2 text-[var(--danger)]">
          {errorMsg}
        </div>
      )}
      {confirmMsg && (
        <div className="rounded-[var(--radius-md)] border border-[var(--success)] bg-[var(--success-surface)] px-3 py-2 text-[var(--success)]">
          {confirmMsg}
        </div>
      )}

      {!sessionId && (
        <div className="rounded-[var(--radius-md)] border border-[var(--border-subtle)] bg-[var(--surface-app)] px-3 py-2 text-[var(--text-tertiary)]">
          {d.noSessionHint}
        </div>
      )}

      <div className="space-y-2">
        <div className="flex items-center justify-between">
          <div className="font-medium text-[var(--text-primary)]">
            {listData
              ? d.listHeader(listData.total)
              : d.listHeader(0)}
          </div>
          <div className="flex items-center gap-1">
            <Button
              variant="secondary"
              size="sm"
              disabled={busy || page <= 1}
              onClick={() => void loadList(page - 1)}
            >
              {d.prevPage}
            </Button>
            <span className="px-1 font-mono text-[11px] text-[var(--text-tertiary)]">
              {page}
            </span>
            <Button
              variant="secondary"
              size="sm"
              disabled={busy || !listData?.next_page}
              onClick={() => void loadList(page + 1)}
            >
              {d.nextPage}
            </Button>
            <Button
              variant="secondary"
              size="sm"
              disabled={busy}
              onClick={() => void loadList(page)}
            >
              <RefreshCw className="h-3.5 w-3.5" strokeWidth={1.75} />
            </Button>
          </div>
        </div>

        {listData && listData.checkpoints.length === 0 && (
          <div className="rounded-[var(--radius-lg)] border border-[var(--border-subtle)] bg-[var(--surface-app)] px-3 py-4 text-center text-[var(--text-tertiary)]">
            {d.empty}
          </div>
        )}

        {listData?.checkpoints.map((summary) => (
          <div
            key={summary.token.id}
            className="rounded-[var(--radius-lg)] border border-[var(--border-subtle)] bg-[var(--surface-raised)] p-3"
          >
            <div className="flex items-start justify-between gap-2">
              <div className="min-w-0 space-y-1">
                <div className="truncate font-mono text-[11px] text-[var(--text-primary)]">
                  {summary.token.id}
                </div>
                <div className="text-[11px] text-[var(--text-tertiary)]">
                  {d.summaryMeta(
                    summary.source.revision,
                    summary.source.label,
                    summary.files,
                    formatBytes(summary.bytes)
                  )}
                </div>
              </div>
              <div className="flex shrink-0 gap-1">
                <Button
                  variant="secondary"
                  size="sm"
                  disabled={busy}
                  onClick={() => void loadDiff(summary)}
                >
                  <GitCompareArrows className="h-3.5 w-3.5" strokeWidth={1.75} />
                  {d.diffBtn}
                </Button>
                <Button
                  variant="secondary"
                  size="sm"
                  disabled={busy || runActive}
                  onClick={() => void handlePrepare("restore", summary)}
                >
                  {d.restoreBtn}
                </Button>
                <Button
                  variant="secondary"
                  size="sm"
                  disabled={busy || runActive}
                  onClick={() => void handlePrepare("accept", summary)}
                >
                  {d.acceptBtn}
                </Button>
              </div>
            </div>
            {diffFor === summary.token.id && diffData && (
              <div className="mt-2 max-h-56 overflow-auto rounded-[var(--radius-md)] border border-[var(--border)] bg-[var(--surface-code)] p-2">
                {diffData.files.length === 0 && (
                  <div className="py-2 text-center text-[11px] text-[var(--text-tertiary)]">
                    {d.noDiff}
                  </div>
                )}
                {diffData.files.map((file) => (
                  <div key={file.path} className="mb-2 last:mb-0">
                    <div className="font-mono text-[11px] text-[var(--accent-text)]">
                      {file.change} · {file.path}
                      {file.preview_truncated ? " · …" : ""}
                    </div>
                    {file.unified_diff && (
                      <pre className="mt-0.5 overflow-x-auto whitespace-pre font-mono text-[10.5px] leading-[1.5] text-[var(--text-secondary)]">
                        {file.unified_diff}
                      </pre>
                    )}
                  </div>
                ))}
                {diffData.preview_truncated && (
                  <div className="pt-1 text-[10.5px] text-[var(--warning)]">
                    {d.truncatedHint}
                  </div>
                )}
              </div>
            )}
          </div>
        ))}
      </div>

      <div className="space-y-2 rounded-[var(--radius-lg)] border border-[var(--border-subtle)] bg-[var(--surface-app)] p-3">
        <div className="font-medium text-[var(--text-primary)]">
          {d.createTitle}
        </div>
        <div className="flex gap-2">
          <input
            type="text"
            aria-label={d.createPathsLabel}
            value={createPaths}
            disabled={runActive || busy}
            onChange={(e) => setCreatePaths(e.target.value)}
            placeholder={d.createPlaceholder}
            className="soyo-input flex-1 px-2.5 py-1.5 font-mono text-xs"
          />
          <Button
            variant="primary"
            size="md"
            disabled={runActive || busy || !createPaths.trim()}
            onClick={() => void handlePrepare("create")}
          >
            {busy && <SquaresLoader size={12} />}
            {d.createBtn}
          </Button>
        </div>

        {prepared && (
          <div className="soyo-panel-enter mt-2 space-y-2 rounded-[var(--radius-md)] border border-[var(--warning)] bg-[var(--warning-surface)] p-3">
            <div className="flex items-center justify-between text-[11px]">
              <span className="font-mono text-[var(--warning)]">
                {d.operationSummary(prepared.operation_id)}
              </span>
            </div>
            {isRestoreReview && (
              <div className="space-y-1">
                <div className="text-[11px] font-medium text-[var(--text-primary)]">
                  {d.approveFilesLabel}
                </div>
                <div className="max-h-32 space-y-1 overflow-auto">
                  {reviewChanges.map((change) => (
                    <label
                      key={change.path}
                      className="flex cursor-pointer items-center gap-2 font-mono text-[11px] text-[var(--text-secondary)]"
                    >
                      <input
                        type="checkbox"
                        checked={approvedPaths.has(change.path)}
                        onChange={() => toggleApproved(change.path)}
                        className="h-3.5 w-3.5"
                      />
                      {change.path}
                    </label>
                  ))}
                </div>
              </div>
            )}
            <pre className="max-h-40 overflow-auto rounded-[var(--radius-md)] border border-[var(--border)] bg-[var(--surface-code)] p-2 font-mono text-[11px] text-[var(--text-primary)]">
              {JSON.stringify(prepared.review, null, 2)}
            </pre>
            <div className="flex justify-end gap-2">
              <Button
                variant="secondary"
                size="sm"
                onClick={() => {
                  setPrepared(null);
                  setApprovedPaths(new Set());
                }}
              >
                {d.discardBtn}
              </Button>
              <Button
                variant="secondary"
                size="sm"
                disabled={busy || (isRestoreReview && approvedPaths.size === 0)}
                onClick={() => void handleConfirm()}
                className="font-semibold text-[var(--success)]"
              >
                {busy ? (
                  <SquaresLoader size={12} />
                ) : (
                  <Check className="h-3.5 w-3.5" strokeWidth={1.75} />
                )}
                {d.confirmBtn}
              </Button>
            </div>
          </div>
        )}
      </div>

      <div className="space-y-2 rounded-[var(--radius-lg)] border border-[var(--border-subtle)] bg-[var(--surface-app)] p-3">
        <div className="flex items-center justify-between">
          <div className="font-medium text-[var(--text-primary)]">
            {d.recoverTitle}
          </div>
          <Button
            variant="secondary"
            size="sm"
            disabled={busy || runActive}
            onClick={() => void handleRecover()}
          >
            <History className="h-3.5 w-3.5" strokeWidth={1.75} />
            {d.recoverBtn}
          </Button>
        </div>
        <div className="text-[11px] text-[var(--text-tertiary)]">
          {d.recoverDesc}
        </div>
        {recoverData && (
          <div className="space-y-1">
            {recoverData.outcomes.length === 0 && (
              <div className="py-1 text-[11px] text-[var(--text-tertiary)]">
                {d.recoverEmpty}
              </div>
            )}
            {recoverData.outcomes.map((outcome) => (
              <div
                key={outcome.operation_id}
                className="rounded-[var(--radius-md)] border border-[var(--border-subtle)] bg-[var(--surface-raised)] p-2 font-mono text-[11px]"
              >
                <div className="flex items-center justify-between">
                  <span className="text-[var(--text-primary)]">
                    {outcome.operation_id}
                  </span>
                  <span
                    className={
                      outcome.status === "completed"
                        ? "text-[var(--success)]"
                        : "text-[var(--warning)]"
                    }
                  >
                    {outcome.status}
                  </span>
                </div>
                <div className="mt-0.5 text-[10.5px] text-[var(--text-tertiary)]">
                  {outcome.files
                    .map((file) => `${file.status}:${file.path}`)
                    .join(" · ")}
                </div>
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );

  if (embedded) {
    return <div data-testid="checkpoints-embedded-panel">{body}</div>;
  }

  return (
    <DialogShell
      open={open}
      onClose={onClose}
      ariaLabel={d.title}
      testId="checkpoints-dialog"
      size="md"
      panelClassName="flex max-h-[85vh] max-w-[720px] flex-col"
    >
      <div className="flex items-center justify-between border-b border-[var(--border-subtle)] px-4 py-3">
        <div className="flex items-center gap-2">
          <History className="h-4 w-4 text-[var(--accent-text)]" strokeWidth={1.75} />
          <h2 className="text-[14px] font-semibold text-[var(--text-primary)]">
            {d.title}
          </h2>
        </div>
        <button
          type="button"
          onClick={onClose}
          aria-label={t.a11y.closeCheckpointsDialog}
          className="rounded-[var(--radius-sm)] p-1 text-[var(--text-tertiary)] transition-colors hover:bg-[var(--surface-hover)] hover:text-[var(--text-primary)]"
        >
          <X className="h-4 w-4" strokeWidth={1.75} />
        </button>
      </div>

      <div className="flex-1 overflow-y-auto p-4">{body}</div>
    </DialogShell>
  );
}
