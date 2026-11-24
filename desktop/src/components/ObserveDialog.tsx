import { Activity, AlertTriangle, GitBranch, ListTree, RefreshCw, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { commands } from "../bindings";
import { getMessages } from "../lib/i18n";
import { useAppStore } from "../store/useAppStore";
import { MarkdownRenderer } from "./MarkdownRenderer";
import { SquaresLoader } from "./SquaresLoader";
import { Button } from "./ui/Button";
import { DialogShell } from "./ui/DialogShell";

export interface ObserveDialogProps {
  open: boolean;
  embedded?: boolean;
  sessionId: string | null;
  onClose: () => void;
}

type ObserveView = "usage" | "timeline" | "diagnose" | "report";

interface ObserveTotals {
  runs: number;
  requests: number;
  input_tokens: number;
  output_tokens: number;
  cached_input_tokens: number;
  cost: Record<string, number>;
  estimated: boolean;
}

interface ObserveGroup {
  dimension: string;
  key: string;
  totals: ObserveTotals;
}

interface ObserveUsageData {
  session_id: string | null;
  totals: ObserveTotals;
  groups: ObserveGroup[];
}

interface TimelineStep {
  index: number;
  seq: number;
  kind: string;
  run_id: string | null;
  label: string;
  input: unknown;
  output: unknown;
  status: string;
  error_code: string | null;
}

interface ObserveTimelineData {
  session_id: string;
  steps: TimelineStep[];
  runs: string[];
}

interface ObserveDiagnosisData {
  session_id: string;
  run_id: string | null;
  failed_step: number | null;
  last_ok_step: number | null;
  error_code: string | null;
  summary: string;
  run_result: unknown;
  step_count: number;
}

interface ObserveReportData {
  session_id: string;
  run_id: string;
  diagnosis: ObserveDiagnosisData;
}

const STATUS_TONE: Record<string, string> = {
  ok: "text-[var(--success)]",
  failed: "text-[var(--danger)]",
  denied: "text-[var(--warning)]",
  cancelled: "text-[var(--warning)]",
  unknown: "text-[var(--text-tertiary)]",
};

function formatCost(cost: Record<string, number>, estimated: boolean): string {
  const entries = Object.entries(cost ?? {});
  if (entries.length === 0) return "—";
  return entries
    .map(([currency, micro]) => {
      const value = (micro / 1_000_000).toFixed(6);
      return `${estimated ? "~" : ""}${currency} ${value}`;
    })
    .join(", ");
}

function pretty(value: unknown): string {
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

/// Preview-only initial view (`?observeView=timeline`), mirroring the existing
/// `scene`／`tab` preview parameters. Ignored outside the dev preview.
function initialView(): ObserveView {
  if (typeof window === "undefined") return "usage";
  const value = new URLSearchParams(window.location.search).get("observeView");
  return value === "timeline" || value === "diagnose" || value === "report"
    ? value
    : "usage";
}

export function ObserveDialog({
  open,
  embedded = false,
  sessionId,
  onClose,
}: ObserveDialogProps) {
  const locale = useAppStore((s) => s.locale);
  const t = getMessages(locale);
  const d = t.observeDialog;

  const [view, setView] = useState<ObserveView>("usage");
  const [runFilter, setRunFilter] = useState("");
  const [usage, setUsage] = useState<ObserveUsageData | null>(null);
  const [timeline, setTimeline] = useState<ObserveTimelineData | null>(null);
  const [diagnosis, setDiagnosis] = useState<ObserveDiagnosisData | null>(null);
  const [reportMarkdown, setReportMarkdown] = useState<string | null>(null);
  const [reportData, setReportData] = useState<ObserveReportData | null>(null);
  const [expandedStep, setExpandedStep] = useState<number | null>(null);
  const [errorMsg, setErrorMsg] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const run = runFilter.trim() ? runFilter.trim() : null;

  const load = useCallback(
    async (target: ObserveView) => {
      if (!sessionId) return;
      setErrorMsg(null);
      setBusy(true);
      try {
        if (target === "usage") {
          const res = await commands.observeUsage(sessionId);
          if (res.status === "ok") {
            setUsage(res.data.data as unknown as ObserveUsageData);
          } else {
            setErrorMsg(`${res.error.code}: ${res.error.message}`);
          }
        } else if (target === "timeline") {
          const res = await commands.observeTimeline(sessionId, run);
          if (res.status === "ok") {
            setTimeline(res.data.data as unknown as ObserveTimelineData);
            setExpandedStep(null);
          } else {
            setErrorMsg(`${res.error.code}: ${res.error.message}`);
          }
        } else if (target === "diagnose") {
          const res = await commands.observeDiagnose(sessionId, run);
          if (res.status === "ok") {
            setDiagnosis(res.data.data as unknown as ObserveDiagnosisData);
          } else {
            setErrorMsg(`${res.error.code}: ${res.error.message}`);
          }
        } else {
          const res = await commands.observeReport(sessionId, run);
          if (res.status === "ok") {
            setReportData(res.data.data as unknown as ObserveReportData);
            setReportMarkdown(res.data.markdown);
          } else {
            setErrorMsg(`${res.error.code}: ${res.error.message}`);
          }
        }
      } finally {
        setBusy(false);
      }
    },
    [sessionId, run]
  );

  useEffect(() => {
    if (!open || !sessionId) return;
    setUsage(null);
    setTimeline(null);
    setDiagnosis(null);
    setReportMarkdown(null);
    setReportData(null);
    setRunFilter("");
    const initial = initialView();
    setView(initial);
    void load(initial);
    // The initial view is fixed to usage; reload only when the dialog opens.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, sessionId]);

  const views = useMemo(
    () =>
      [
        { id: "usage" as const, label: d.viewUsage, icon: Activity },
        { id: "timeline" as const, label: d.viewTimeline, icon: ListTree },
        { id: "diagnose" as const, label: d.viewDiagnose, icon: AlertTriangle },
        { id: "report" as const, label: d.viewReport, icon: GitBranch },
      ] satisfies { id: ObserveView; label: string; icon: typeof Activity }[],
    [d]
  );

  if (!open) {
    return null;
  }

  const usageTotal = usage?.totals;
  const groups = usage?.groups ?? [];

  const body = (
    <div className="space-y-3 text-xs">
      {errorMsg && (
        <div className="rounded-[var(--radius-md)] border border-[var(--danger)] bg-[var(--danger-surface)] px-3 py-2 text-[var(--danger)]">
          {errorMsg}
        </div>
      )}

      {!sessionId && (
        <div className="rounded-[var(--radius-md)] border border-[var(--border-subtle)] bg-[var(--surface-app)] px-3 py-2 text-[var(--text-tertiary)]">
          {d.noSessionHint}
        </div>
      )}

      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex flex-wrap gap-1">
          {views.map(({ id, label, icon: Icon }) => (
            <button
              key={id}
              type="button"
              data-testid={`observe-view-${id}`}
              aria-current={view === id ? "page" : undefined}
              onClick={() => {
                setView(id);
                void load(id);
              }}
              disabled={!sessionId || busy}
              className={`flex items-center gap-1.5 rounded-[var(--radius-md)] border px-2.5 py-1 transition-colors ${
                view === id
                  ? "border-[var(--accent-border)] bg-[var(--accent-surface)] text-[var(--accent-text)]"
                  : "border-[var(--border-subtle)] text-[var(--text-secondary)] hover:bg-[var(--surface-hover)]"
              }`}
            >
              <Icon className="h-3.5 w-3.5" strokeWidth={1.75} />
              {label}
            </button>
          ))}
        </div>
        <div className="flex items-center gap-1.5">
          {view !== "usage" && (
            <input
              type="text"
              aria-label={d.runFilterLabel}
              value={runFilter}
              onChange={(e) => setRunFilter(e.target.value)}
              placeholder={d.runFilterPlaceholder}
              className="soyo-input w-[168px] px-2 py-1 font-mono text-[11px]"
            />
          )}
          <Button
            variant="secondary"
            size="sm"
            disabled={!sessionId || busy}
            onClick={() => void load(view)}
          >
            <RefreshCw className="h-3.5 w-3.5" strokeWidth={1.75} />
          </Button>
        </div>
      </div>

      <div className="text-[11px] text-[var(--text-tertiary)]">{d.readOnlyHint}</div>

      {busy && (
        <div className="flex items-center gap-2 py-4 text-[var(--text-tertiary)]">
          <SquaresLoader size={12} />
          {d.loading}
        </div>
      )}

      {!busy && view === "usage" && usageTotal && (
        <div className="space-y-2" data-testid="observe-usage-panel">
          <div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
            {[
              [d.colRuns, String(usageTotal.runs)],
              [d.colRequests, String(usageTotal.requests)],
              [d.colTokens, `${usageTotal.input_tokens} / ${usageTotal.output_tokens}`],
              [d.colCost, formatCost(usageTotal.cost, usageTotal.estimated)],
            ].map(([label, value]) => (
              <div
                key={label}
                className="rounded-[var(--radius-md)] border border-[var(--border-subtle)] bg-[var(--surface-raised)] px-3 py-2"
              >
                <div className="text-[10.5px] text-[var(--text-tertiary)]">{label}</div>
                <div className="mt-0.5 font-mono text-[12px] text-[var(--text-primary)]">
                  {value}
                </div>
              </div>
            ))}
          </div>
          {groups.length === 0 ? (
            <div className="rounded-[var(--radius-lg)] border border-[var(--border-subtle)] bg-[var(--surface-app)] px-3 py-4 text-center text-[var(--text-tertiary)]">
              {d.usageEmpty}
            </div>
          ) : (
            <div className="overflow-auto rounded-[var(--radius-lg)] border border-[var(--border-subtle)]">
              <table className="w-full border-collapse text-left text-[11px]">
                <thead className="bg-[var(--surface-app)] text-[var(--text-tertiary)]">
                  <tr>
                    <th className="px-2 py-1.5 font-medium">{d.colScope}</th>
                    <th className="px-2 py-1.5 font-medium">{d.colKey}</th>
                    <th className="px-2 py-1.5 text-right font-medium">{d.colRequests}</th>
                    <th className="px-2 py-1.5 text-right font-medium">{d.colTokens}</th>
                    <th className="px-2 py-1.5 text-right font-medium">{d.colCost}</th>
                  </tr>
                </thead>
                <tbody className="font-mono">
                  {groups.map((group) => (
                    <tr
                      key={`${group.dimension}:${group.key}`}
                      className="border-t border-[var(--border-subtle)]"
                    >
                      <td className="px-2 py-1.5 text-[var(--text-tertiary)]">
                        {group.dimension}
                      </td>
                      <td className="px-2 py-1.5 text-[var(--text-primary)]">{group.key}</td>
                      <td className="px-2 py-1.5 text-right">{group.totals.requests}</td>
                      <td className="px-2 py-1.5 text-right">
                        {group.totals.input_tokens} / {group.totals.output_tokens}
                      </td>
                      <td className="px-2 py-1.5 text-right">
                        {formatCost(group.totals.cost, group.totals.estimated)}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </div>
      )}

      {!busy && view === "timeline" && timeline && (
        <div className="space-y-1" data-testid="observe-timeline-panel">
          <div className="text-[11px] text-[var(--text-tertiary)]">
            {d.timelineSummary(timeline.steps.length, timeline.runs.length)}
          </div>
          {timeline.steps.length === 0 && (
            <div className="rounded-[var(--radius-lg)] border border-[var(--border-subtle)] bg-[var(--surface-app)] px-3 py-4 text-center text-[var(--text-tertiary)]">
              {d.timelineEmpty}
            </div>
          )}
          {timeline.steps.map((step) => (
            <div
              key={step.index}
              className="rounded-[var(--radius-md)] border border-[var(--border-subtle)] bg-[var(--surface-raised)]"
            >
              <button
                type="button"
                className="flex w-full items-center gap-2 px-2.5 py-1.5 text-left font-mono text-[11px] hover:bg-[var(--surface-hover)]"
                onClick={() =>
                  setExpandedStep(expandedStep === step.index ? null : step.index)
                }
              >
                <span className="w-10 shrink-0 text-right text-[var(--text-tertiary)]">
                  {step.index}
                </span>
                <span
                  className={`w-[58px] shrink-0 uppercase ${
                    STATUS_TONE[step.status] ?? "text-[var(--text-secondary)]"
                  }`}
                >
                  {step.status}
                </span>
                <span className="w-[128px] shrink-0 truncate text-[var(--text-tertiary)]">
                  {step.kind}
                </span>
                <span className="min-w-0 flex-1 truncate text-[var(--text-primary)]">
                  {step.label}
                </span>
                {step.error_code && (
                  <span className="shrink-0 text-[var(--danger)]">[{step.error_code}]</span>
                )}
              </button>
              {expandedStep === step.index && (
                <div className="grid gap-2 border-t border-[var(--border-subtle)] p-2 sm:grid-cols-2">
                  <div>
                    <div className="mb-1 text-[10.5px] font-medium text-[var(--text-tertiary)]">
                      {d.stepInput}
                    </div>
                    <pre className="max-h-48 overflow-auto rounded-[var(--radius-md)] border border-[var(--border)] bg-[var(--surface-code)] p-2 font-mono text-[10.5px] text-[var(--text-secondary)]">
                      {pretty(step.input)}
                    </pre>
                  </div>
                  <div>
                    <div className="mb-1 text-[10.5px] font-medium text-[var(--text-tertiary)]">
                      {d.stepOutput}
                    </div>
                    <pre className="max-h-48 overflow-auto rounded-[var(--radius-md)] border border-[var(--border)] bg-[var(--surface-code)] p-2 font-mono text-[10.5px] text-[var(--text-secondary)]">
                      {pretty(step.output)}
                    </pre>
                  </div>
                </div>
              )}
            </div>
          ))}
        </div>
      )}

      {!busy && view === "diagnose" && diagnosis && (
        <div className="space-y-2" data-testid="observe-diagnose-panel">
          <div
            className={`rounded-[var(--radius-md)] border px-3 py-2 ${
              diagnosis.error_code
                ? "border-[var(--danger)] bg-[var(--danger-surface)] text-[var(--danger)]"
                : "border-[var(--success)] bg-[var(--success-surface)] text-[var(--success)]"
            }`}
          >
            {diagnosis.summary}
          </div>
          <div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
            {[
              [d.colSteps, String(diagnosis.step_count)],
              [d.colFailedStep, diagnosis.failed_step === null ? "—" : String(diagnosis.failed_step)],
              [d.colLastOk, diagnosis.last_ok_step === null ? "—" : String(diagnosis.last_ok_step)],
              [d.colErrorCode, diagnosis.error_code ?? "—"],
            ].map(([label, value]) => (
              <div
                key={label}
                className="rounded-[var(--radius-md)] border border-[var(--border-subtle)] bg-[var(--surface-raised)] px-3 py-2"
              >
                <div className="text-[10.5px] text-[var(--text-tertiary)]">{label}</div>
                <div className="mt-0.5 break-all font-mono text-[12px] text-[var(--text-primary)]">
                  {value}
                </div>
              </div>
            ))}
          </div>
          {diagnosis.run_result !== null && (
            <div>
              <div className="mb-1 text-[10.5px] font-medium text-[var(--text-tertiary)]">
                {d.runResult}
              </div>
              <pre className="max-h-56 overflow-auto rounded-[var(--radius-md)] border border-[var(--border)] bg-[var(--surface-code)] p-2 font-mono text-[10.5px] text-[var(--text-secondary)]">
                {pretty(diagnosis.run_result)}
              </pre>
            </div>
          )}
        </div>
      )}

      {!busy && view === "report" && (
        <div className="space-y-2" data-testid="observe-report-panel">
          {reportData && (
            <div className="text-[11px] text-[var(--text-tertiary)]">
              {d.reportMeta(reportData.run_id, reportData.diagnosis.step_count)}
            </div>
          )}
          {reportMarkdown ? (
            <div className="max-h-[46vh] overflow-auto rounded-[var(--radius-md)] border border-[var(--border)] bg-[var(--surface-code)] p-3">
              <MarkdownRenderer content={reportMarkdown} mode="render" />
            </div>
          ) : (
            <div className="rounded-[var(--radius-lg)] border border-[var(--border-subtle)] bg-[var(--surface-app)] px-3 py-4 text-center text-[var(--text-tertiary)]">
              {d.reportEmpty}
            </div>
          )}
        </div>
      )}
    </div>
  );

  if (embedded) {
    return <div data-testid="observe-embedded-panel">{body}</div>;
  }

  return (
    <DialogShell
      open={open}
      onClose={onClose}
      ariaLabel={d.title}
      testId="observe-dialog"
      size="md"
      panelClassName="flex max-h-[85vh] max-w-[760px] flex-col"
    >
      <div className="flex items-center justify-between border-b border-[var(--border-subtle)] px-4 py-3">
        <div className="flex items-center gap-2">
          <Activity className="h-4 w-4 text-[var(--accent-text)]" strokeWidth={1.75} />
          <h2 className="text-[14px] font-semibold text-[var(--text-primary)]">{d.title}</h2>
        </div>
        <button
          type="button"
          onClick={onClose}
          aria-label={t.a11y.closeObserveDialog}
          className="rounded-[var(--radius-sm)] p-1 text-[var(--text-tertiary)] transition-colors hover:bg-[var(--surface-hover)] hover:text-[var(--text-primary)]"
        >
          <X className="h-4 w-4" strokeWidth={1.75} />
        </button>
      </div>

      <div className="flex-1 overflow-y-auto p-4">{body}</div>
    </DialogShell>
  );
}
