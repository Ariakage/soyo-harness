import {
  CheckCircle2,
  ChevronLeft,
  Flag,
  ListChecks,
  Plus,
  RefreshCw,
  X,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import {
  commands,
  type GoalBudgetDto,
  type GoalDto_Serialize,
  type GoalJobDto_Serialize,
  type GoalStepDto_Serialize,
} from "../bindings";
import { getMessages } from "../lib/i18n";
import { sanitizeDisplayText } from "../lib/sanitize";
import { useAppStore } from "../store/useAppStore";
import { Button } from "./ui/Button";
import { DialogShell } from "./ui/DialogShell";
import { Pill } from "./ui/Pill";

export interface GoalsDialogProps {
  open: boolean;
  onClose: () => void;
}

const FINISHED = new Set(["completed", "failed", "cancelled", "limit_reached"]);
const DEFAULT_BUDGET: GoalBudgetDto = {
  max_runs: 32,
  max_model_requests: 512,
  max_tool_calls: 4096,
  max_active_seconds: 86400,
  max_tokens: 8000000,
  max_planning_attempts: 3,
  max_verification_attempts: 8,
  max_replans: 2,
};

function statusTone(status: string): "neutral" | "info" | "success" | "warning" | "danger" | "accent" {
  switch (status) {
    case "running":
      return "info";
    case "completed":
      return "success";
    case "failed":
    case "cancelled":
      return "danger";
    case "blocked":
    case "limit_reached":
    case "waiting":
      return "warning";
    case "ready":
      return "accent";
    default:
      return "neutral";
  }
}

export function GoalsDialog({ open, onClose }: GoalsDialogProps) {
  const locale = useAppStore((s) => s.locale);
  const t = getMessages(locale);
  const g = t.goals;

  const [goals, setGoals] = useState<GoalDto_Serialize[]>([]);
  const [jobs, setJobs] = useState<GoalJobDto_Serialize[]>([]);
  const [selected, setSelected] = useState<GoalDto_Serialize | null>(null);
  const [creating, setCreating] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // create form
  const [objective, setObjective] = useState("");
  const [criteria, setCriteria] = useState<string[]>([""]);
  const [showBudget, setShowBudget] = useState(false);
  const [budget, setBudget] = useState<GoalBudgetDto>({ ...DEFAULT_BUDGET });

  // sub-dialogs
  const [verifyOpen, setVerifyOpen] = useState(false);
  const [verifyCriterion, setVerifyCriterion] = useState("");
  const [verifySummary, setVerifySummary] = useState("");
  const [resolveStep, setResolveStep] = useState<GoalStepDto_Serialize | null>(null);
  const [verifierStep, setVerifierStep] = useState<GoalStepDto_Serialize | null>(null);
  const [verifierArgv, setVerifierArgv] = useState("");
  const [verifierCwd, setVerifierCwd] = useState("");
  const [verifierTimeout, setVerifierTimeout] = useState("60");
  const [verifierMaxOut, setVerifierMaxOut] = useState("8192");
  const [report, setReport] = useState<{ format: string; content: string } | null>(null);
  const [reportLoading, setReportLoading] = useState(false);

  const statusLabel = useMemo(
    () =>
      ({
        draft: g.statusDraft,
        ready: g.statusReady,
        running: g.statusRunning,
        waiting: g.statusWaiting,
        paused: g.statusPaused,
        blocked: g.statusBlocked,
        completed: g.statusCompleted,
        failed: g.statusFailed,
        cancelled: g.statusCancelled,
        limit_reached: g.statusLimitReached,
      }) as Record<string, string>,
    [g],
  );
  const phaseLabel = useMemo(
    () =>
      ({
        planning: g.phasePlanning,
        executing: g.phaseExecuting,
        verifying: g.phaseVerifying,
        replanning: g.phaseReplanning,
        finished: g.phaseFinished,
      }) as Record<string, string>,
    [g],
  );

  const refresh = useCallback(async (keepSelection = true) => {
    const [listRes, jobRes] = await Promise.all([
      commands.goalList(),
      commands.goalJobs(),
    ]);
    if (listRes.status === "ok" && Array.isArray(listRes.data)) {
      setGoals(listRes.data);
      if (keepSelection) {
        setSelected((prev) => {
          if (!prev) return prev;
          return listRes.data.find((x) => x.goal_id === prev.goal_id) ?? prev;
        });
      }
    }
    if (jobRes.status === "ok" && Array.isArray(jobRes.data)) setJobs(jobRes.data);
  }, []);

  const refreshDetail = useCallback(async (goalId: string) => {
    const res = await commands.goalStatus(goalId);
    if (res.status === "ok") setSelected(res.data);
  }, []);

  useEffect(() => {
    if (!open) return;
    void refresh();
  }, [open, refresh]);

  useEffect(() => {
    if (!open) return;
    const active = jobs.some((j) => j.state === "running") ||
      (selected && !FINISHED.has(selected.status));
    if (!active) return;
    const timer = window.setInterval(() => {
      void refresh();
      if (selected) void refreshDetail(selected.goal_id);
    }, 3000);
    return () => window.clearInterval(timer);
  }, [open, jobs, selected, refresh, refreshDetail]);

  const jobFor = (goalId: string) =>
    jobs.find((j) => j.goal_id === goalId && j.state === "running");

  const runAction = async (
    fn: () => Promise<{ status: string; data?: unknown; error?: { code: string; message: string } }>,
  ) => {
    setBusy(true);
    setError(null);
    const res = await fn();
    if (res.status === "ok") {
      await refresh();
      if (selected) await refreshDetail(selected.goal_id);
    } else if (res.error) {
      setError(`${res.error.code}: ${res.error.message}`);
    }
    setBusy(false);
  };

  const openDetail = async (goal: GoalDto_Serialize) => {
    setSelected(goal);
    await refreshDetail(goal.goal_id);
  };

  const closeAll = () => {
    setSelected(null);
    setCreating(false);
    setVerifyOpen(false);
    setResolveStep(null);
    setVerifierStep(null);
    setReport(null);
    setError(null);
  };

  const submitCreate = async () => {
    const clean = criteria.map((c) => c.trim()).filter(Boolean);
    await runAction(() =>
      commands.goalCreate(objective.trim(), clean, showBudget ? budget : null),
    );
    if (!error) setCreating(false);
  };

  const submitVerify = async () => {
    if (!selected) return;
    await runAction(() =>
      commands.goalVerify(selected.goal_id, verifyCriterion, verifySummary.trim() || null),
    );
    setVerifyOpen(false);
  };

  const submitVerifier = async (clear: boolean) => {
    if (!selected || !verifierStep) return;
    const argv = verifierArgv.split("\n").map((s) => s.trim()).filter(Boolean);
    await runAction(() =>
      commands.goalSetVerifier(
        selected.goal_id,
        verifierStep.step_id,
        clear ? [] : argv,
        verifierCwd.trim() || null,
        Number(verifierTimeout) || null,
        Number(verifierMaxOut) || null,
        clear,
      ),
    );
    setVerifierStep(null);
  };

  const loadReport = async (format: string) => {
    if (!selected) return;
    setReportLoading(true);
    const res = await commands.goalReport(selected.goal_id, format);
    if (res.status === "ok") {
      setReport({ format: res.data.format, content: res.data.content });
    } else {
      setError(`${res.error.code}: ${res.error.message}`);
    }
    setReportLoading(false);
  };

  const goal = selected;
  const runningJob = goal ? jobFor(goal.goal_id) : undefined;
  const blockedByJob = busy || !!runningJob;

  const renderStep = (step: GoalStepDto_Serialize) => (
    <li
      key={step.step_id}
      className="space-y-1 rounded-[var(--radius-md)] border border-[var(--border)] px-2.5 py-2"
      data-testid={`goal-step-${step.step_id}`}
    >
      <div className="flex items-center gap-2">
        <span className="text-[11px] text-[var(--text-tertiary)]">#{step.ordinal}</span>
        <span className="min-w-0 flex-1 truncate text-[var(--text-primary)]">
          {sanitizeDisplayText(step.title)}
        </span>
        <Pill tone={statusTone(step.status)}>{statusLabel[step.status] ?? step.status}</Pill>
        {step.verifier && (
          <Pill tone="accent">{g.verifierBadge}</Pill>
        )}
      </div>
      {step.depends_on.length > 0 && (
        <p className="text-[10px] text-[var(--text-tertiary)]">
          {g.stepDependencies(step.depends_on.join(", "))}
        </p>
      )}
      {step.last_error && (
        <p className="text-[10px] text-[var(--danger)]">{sanitizeDisplayText(step.last_error)}</p>
      )}
      {step.last_verification && (
        <p className="text-[10px] text-[var(--text-tertiary)]">
          {g.lastVerification}: {step.last_verification.status}
          {step.last_verification.error ? ` · ${step.last_verification.error}` : ""}
        </p>
      )}
      <div className="flex gap-1.5 pt-0.5">
        {goal && !goal.verifiers_confirmed && !FINISHED.has(goal.status) && (
          <Button
            size="sm"
            variant="ghost"
            disabled={blockedByJob}
            onClick={() => {
              setVerifierStep(step);
              setVerifierArgv(step.verifier?.argv.join("\n") ?? "");
              setVerifierCwd(step.verifier?.cwd ?? "");
              setVerifierTimeout(String(step.verifier?.timeout_seconds ?? 60));
              setVerifierMaxOut(String(step.verifier?.max_output_bytes ?? 8192));
            }}
          >
            {g.setVerifierAction}
          </Button>
        )}
        {step.status === "failed" && step.last_status === "unknown_side_effects" && goal && (
          <Button
            size="sm"
            variant="ghost"
            disabled={blockedByJob}
            onClick={() => setResolveStep(step)}
          >
            {g.resolveAction}
          </Button>
        )}
      </div>
    </li>
  );

  return (
    <DialogShell
      open={open}
      onClose={() => {
        closeAll();
        onClose();
      }}
      ariaLabel={g.title}
      testId="goals-dialog"
      size="lg"
      panelClassName="flex max-h-[84vh] max-w-[760px] flex-col"
    >
      <div className="flex items-center justify-between border-b border-[var(--border-subtle)] px-4 py-3">
        <div className="flex items-center gap-2">
          <Flag className="h-4 w-4 text-[var(--accent-text)]" strokeWidth={1.75} />
          <h2 className="text-[14px] font-semibold text-[var(--text-primary)]">{g.title}</h2>
          {goal && (
            <Button size="sm" variant="ghost" onClick={() => setSelected(null)}>
              <ChevronLeft className="h-3.5 w-3.5" /> {g.back}
            </Button>
          )}
        </div>
        <div className="flex items-center gap-1.5">
          {!goal && !creating && (
            <Button size="sm" variant="secondary" onClick={() => setCreating(true)}>
              <Plus className="h-3.5 w-3.5" /> {g.create}
            </Button>
          )}
          <Button
            size="sm"
            variant="ghost"
            aria-label={g.refresh}
            onClick={() => void refresh()}
          >
            <RefreshCw className="h-3.5 w-3.5" />
          </Button>
          <button
            type="button"
            onClick={() => {
              closeAll();
              onClose();
            }}
            aria-label={g.title}
            className="rounded-[var(--radius-sm)] p-1 text-[var(--text-tertiary)] transition-colors hover:bg-[var(--surface-hover)] hover:text-[var(--text-primary)]"
          >
            <X className="h-4 w-4" />
          </button>
        </div>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto px-4 py-3 text-xs">
        {error && (
          <div className="mb-3 rounded-[var(--radius-md)] border border-[var(--danger)] bg-[var(--danger-surface)] px-2.5 py-1.5 text-[11px] text-[var(--danger)]">
            {error}
          </div>
        )}

        {/* ── create form ── */}
        {creating && !goal && (
          <div className="space-y-3" data-testid="goal-create-form">
            <label className="block space-y-1">
              <span className="text-[var(--text-secondary)]">{g.objectiveLabel}</span>
              <textarea
                value={objective}
                onChange={(e) => setObjective(e.target.value)}
                placeholder={g.objectivePlaceholder}
                rows={3}
                className="w-full rounded-[var(--radius-md)] border border-[var(--border)] bg-transparent px-2.5 py-1.5 text-[var(--text-primary)] outline-none focus:border-[var(--accent)]"
              />
            </label>
            <div className="space-y-1.5">
              <span className="text-[var(--text-secondary)]">{g.acceptanceLabel}</span>
              {criteria.map((c, i) => (
                <input
                  key={i}
                  value={c}
                  onChange={(e) =>
                    setCriteria(criteria.map((x, j) => (j === i ? e.target.value : x)))
                  }
                  className="w-full rounded-[var(--radius-md)] border border-[var(--border)] bg-transparent px-2.5 py-1.5 text-[var(--text-primary)] outline-none focus:border-[var(--accent)]"
                />
              ))}
              <Button size="sm" variant="ghost" onClick={() => setCriteria([...criteria, ""])}>
                <Plus className="h-3 w-3" /> {g.addCriterion}
              </Button>
            </div>
            <div>
              <Button size="sm" variant="ghost" onClick={() => setShowBudget(!showBudget)}>
                {g.budgetAdvanced}
              </Button>
              {showBudget && (
                <div className="mt-1.5 grid grid-cols-2 gap-2">
                  {(
                    [
                      ["max_runs", g.budgetMaxRuns],
                      ["max_model_requests", g.budgetMaxRequests],
                      ["max_tool_calls", g.budgetMaxTools],
                      ["max_active_seconds", g.budgetMaxActiveSeconds],
                      ["max_tokens", g.budgetMaxTokens],
                      ["max_replans", g.budgetMaxReplans],
                    ] as const
                  ).map(([key, label]) => (
                    <label key={key} className="block space-y-0.5">
                      <span className="text-[10px] text-[var(--text-tertiary)]">{label}</span>
                      <input
                        type="number"
                        value={budget[key]}
                        onChange={(e) =>
                          setBudget({ ...budget, [key]: Number(e.target.value) || 0 })
                        }
                        className="w-full rounded-[var(--radius-md)] border border-[var(--border)] bg-transparent px-2 py-1 text-[var(--text-primary)] outline-none focus:border-[var(--accent)]"
                      />
                    </label>
                  ))}
                </div>
              )}
            </div>
            <div className="flex gap-2">
              <Button
                variant="primary"
                disabled={busy || !objective.trim()}
                onClick={() => void submitCreate()}
              >
                {busy ? g.busy : g.createSubmit}
              </Button>
              <Button variant="secondary" onClick={() => setCreating(false)}>
                {g.back}
              </Button>
            </div>
          </div>
        )}

        {/* ── list ── */}
        {!creating && !goal && (
          <ul className="space-y-2" data-testid="goal-list">
            {goals.length === 0 && (
              <li className="py-6 text-center text-[11px] text-[var(--text-tertiary)]">
                {g.empty}
              </li>
            )}
            {goals.map((item) => (
              <li key={item.goal_id}>
                <button
                  type="button"
                  onClick={() => void openDetail(item)}
                  className="w-full space-y-1.5 rounded-[var(--radius-md)] border border-[var(--border)] px-3 py-2.5 text-left transition-colors hover:border-[var(--border-strong)] hover:bg-[var(--surface-hover)]"
                  data-testid={`goal-item-${item.goal_id}`}
                >
                  <div className="flex items-center gap-2">
                    <span className="min-w-0 flex-1 truncate text-[var(--text-primary)]">
                      {sanitizeDisplayText(item.objective)}
                    </span>
                    <Pill tone={statusTone(item.status)}>
                      {statusLabel[item.status] ?? item.status}
                    </Pill>
                    <Pill>{phaseLabel[item.phase] ?? item.phase}</Pill>
                  </div>
                  <div className="flex items-center gap-3 text-[10px] text-[var(--text-tertiary)]">
                    <span>
                      {item.steps.filter((s) => s.status === "completed").length}/
                      {item.steps.length} · {item.goal_id.slice(0, 8)}
                    </span>
                    {item.running_action && (
                      <span className="text-[var(--info)]">
                        {g.runningAction(item.running_action)}
                      </span>
                    )}
                    <span className="ml-auto">
                      {g.updatedLabel} {new Date(item.updated_at).toLocaleString(locale)}
                    </span>
                  </div>
                </button>
              </li>
            ))}
          </ul>
        )}

        {/* ── detail ── */}
        {goal && (
          <div className="space-y-4" data-testid="goal-detail">
            <div className="space-y-1.5">
              <div className="flex items-start gap-2">
                <p className="min-w-0 flex-1 text-[13px] font-medium text-[var(--text-primary)]">
                  {sanitizeDisplayText(goal.objective)}
                </p>
                <Pill tone={statusTone(goal.status)}>
                  {statusLabel[goal.status] ?? goal.status}
                </Pill>
                <Pill>{phaseLabel[goal.phase] ?? goal.phase}</Pill>
              </div>
              <p className="text-[10px] text-[var(--text-tertiary)]">
                {goal.goal_id} · {goal.workspace} · {goal.permissions}
              </p>
              {runningJob && (
                <p className="text-[11px] text-[var(--info)]">
                  {g.runningAction(runningJob.action)}
                </p>
              )}
              {goal.wait_reason && (
                <p className="text-[11px] text-[var(--warning)]">
                  {g.waitLabel(goal.wait_reason)}
                </p>
              )}
              {goal.failure && (
                <p className="text-[11px] text-[var(--danger)]">
                  {g.failureLabel}: {sanitizeDisplayText(goal.failure)}
                </p>
              )}
              {goal.verifiers_confirmed === false && goal.steps.length > 0 && (
                <p className="text-[11px] text-[var(--warning)]">{g.confirmNeeded}</p>
              )}
            </div>

            <div className="flex flex-wrap gap-1.5">
              {(goal.status === "draft" || goal.phase === "planning") && (
                <Button size="sm" variant="secondary" disabled={blockedByJob}
                  onClick={() => void runAction(() => commands.goalPlan(goal.goal_id, null))}>
                  {g.planAction}
                </Button>
              )}
              {goal.wait_reason === "plan_confirmation" && (
                <Button size="sm" variant="primary" disabled={blockedByJob}
                  onClick={() => void runAction(() => commands.goalConfirm(goal.goal_id))}>
                  {g.confirmAction}
                </Button>
              )}
              {goal.status === "ready" && (
                <>
                  <Button size="sm" variant="primary" disabled={blockedByJob}
                    onClick={() => void runAction(() => commands.goalContinue(goal.goal_id))}>
                    {g.continueAction}
                  </Button>
                  <Button size="sm" variant="secondary" disabled={blockedByJob}
                    onClick={() => void runAction(() => commands.goalStart(goal.goal_id))}>
                    {g.startAction}
                  </Button>
                </>
              )}
              {goal.status === "running" && (
                <Button size="sm" variant="secondary" disabled={blockedByJob}
                  onClick={() => void runAction(() => commands.goalPause(goal.goal_id))}>
                  {g.pauseAction}
                </Button>
              )}
              {(goal.status === "paused" || goal.status === "blocked") && (
                <Button size="sm" variant="secondary" disabled={blockedByJob}
                  onClick={() => void runAction(() => commands.goalResume(goal.goal_id))}>
                  {g.resumeAction}
                </Button>
              )}
              {(goal.phase === "verifying" || goal.wait_reason === "verification") && (
                <Button size="sm" variant="secondary" disabled={blockedByJob}
                  onClick={() => {
                    setVerifyCriterion(goal.acceptance.find((c) => !c.verified)?.id ?? "");
                    setVerifySummary("");
                    setVerifyOpen(true);
                  }}>
                  {g.verifyAction}
                </Button>
              )}
              {goal.steps.some((s) => s.status === "failed") && (
                <Button size="sm" variant="secondary" disabled={blockedByJob}
                  onClick={() => void runAction(() => commands.goalReplan(goal.goal_id))}>
                  {g.replanAction}
                </Button>
              )}
              <Button size="sm" variant="ghost" disabled={reportLoading}
                onClick={() => void loadReport("markdown")}>
                {g.reportAction}
              </Button>
              {!FINISHED.has(goal.status) && (
                <Button size="sm" variant="danger" disabled={blockedByJob}
                  onClick={() => void runAction(() => commands.goalCancel(goal.goal_id))}>
                  {g.cancelAction}
                </Button>
              )}
            </div>

            <section className="space-y-1.5">
              <div className="flex items-center gap-1.5 font-medium text-[var(--text-primary)]">
                <ListChecks className="h-3.5 w-3.5 text-[var(--accent-text)]" strokeWidth={1.75} />
                <span>{g.stepsHeader(goal.steps.length)}</span>
              </div>
              <ul className="space-y-1.5">{goal.steps.map(renderStep)}</ul>
            </section>

            <section className="space-y-1.5">
              <div className="flex items-center gap-1.5 font-medium text-[var(--text-primary)]">
                <CheckCircle2 className="h-3.5 w-3.5 text-[var(--accent-text)]" strokeWidth={1.75} />
                <span>{g.criteriaHeader(goal.acceptance.length)}</span>
              </div>
              <ul className="space-y-1">
                {goal.acceptance.map((c) => (
                  <li key={c.id} className="flex items-center gap-2 text-[11px]">
                    <Pill tone={c.verified ? "success" : "neutral"}>
                      {c.verified ? "✓" : c.verification}
                    </Pill>
                    <span className="text-[var(--text-secondary)]">
                      {sanitizeDisplayText(c.text)}
                    </span>
                  </li>
                ))}
              </ul>
            </section>

            <section className="space-y-1">
              <span className="font-medium text-[var(--text-primary)]">{g.usageHeader}</span>
              <div className="grid grid-cols-2 gap-x-4 gap-y-0.5 text-[11px] text-[var(--text-secondary)]">
                <span>{g.runsLabel}: {goal.usage.runs}/{goal.budget.max_runs}</span>
                <span>{g.requestsLabel}: {goal.usage.model_requests}/{goal.budget.max_model_requests}</span>
                <span>{g.toolsLabel}: {goal.usage.tool_calls}/{goal.budget.max_tool_calls}</span>
                <span>{g.activeTimeLabel}: {goal.usage.active_seconds}s/{goal.budget.max_active_seconds}s</span>
                <span>{g.tokensLabel}: {goal.usage.input_tokens + goal.usage.output_tokens}/{goal.budget.max_tokens}</span>
              </div>
            </section>
          </div>
        )}
      </div>

      {/* ── verify ── */}
      {verifyOpen && goal && (
        <div className="border-t border-[var(--border-subtle)] px-4 py-3" data-testid="goal-verify-form">
          <div className="space-y-2">
            <span className="text-[12px] font-medium text-[var(--text-primary)]">{g.verifyTitle}</span>
            <label className="block space-y-0.5">
              <span className="text-[10px] text-[var(--text-tertiary)]">{g.verifyCriterionLabel}</span>
              <select
                value={verifyCriterion}
                onChange={(e) => setVerifyCriterion(e.target.value)}
                className="w-full rounded-[var(--radius-md)] border border-[var(--border)] bg-transparent px-2 py-1 text-[var(--text-primary)]"
              >
                {goal.acceptance.filter((c) => !c.verified).map((c) => (
                  <option key={c.id} value={c.id}>{c.id}</option>
                ))}
              </select>
            </label>
            <label className="block space-y-0.5">
              <span className="text-[10px] text-[var(--text-tertiary)]">{g.verifySummaryLabel}</span>
              <textarea
                value={verifySummary}
                onChange={(e) => setVerifySummary(e.target.value)}
                placeholder={g.verifySummaryPlaceholder}
                rows={2}
                className="w-full rounded-[var(--radius-md)] border border-[var(--border)] bg-transparent px-2.5 py-1.5 text-[var(--text-primary)] outline-none focus:border-[var(--accent)]"
              />
            </label>
            <div className="flex gap-2">
              <Button size="sm" variant="primary" disabled={busy || !verifyCriterion}
                onClick={() => void submitVerify()}>
                {g.verifySubmit}
              </Button>
              <Button size="sm" variant="secondary" onClick={() => setVerifyOpen(false)}>
                {g.back}
              </Button>
            </div>
          </div>
        </div>
      )}

      {/* ── resolve ── */}
      {resolveStep && goal && (
        <div className="border-t border-[var(--border-subtle)] px-4 py-3">
          <div className="flex items-center gap-2">
            <span className="flex-1 text-[12px] font-medium text-[var(--text-primary)]">
              {g.resolveTitle} — {sanitizeDisplayText(resolveStep.title)}
            </span>
            <Button size="sm" variant="secondary" disabled={blockedByJob}
              onClick={() => {
                void runAction(() => commands.goalResolve(goal.goal_id, resolveStep.step_id, true));
                setResolveStep(null);
              }}>
              {g.resolveRetry}
            </Button>
            <Button size="sm" variant="secondary" disabled={blockedByJob}
              onClick={() => {
                void runAction(() => commands.goalResolve(goal.goal_id, resolveStep.step_id, false));
                setResolveStep(null);
              }}>
              {g.resolveComplete}
            </Button>
            <Button size="sm" variant="ghost" onClick={() => setResolveStep(null)}>
              <X className="h-3.5 w-3.5" />
            </Button>
          </div>
        </div>
      )}

      {/* ── verifier ── */}
      {verifierStep && goal && (
        <div className="border-t border-[var(--border-subtle)] px-4 py-3" data-testid="goal-verifier-form">
          <div className="space-y-2">
            <span className="text-[12px] font-medium text-[var(--text-primary)]">
              {g.verifierTitle} — {sanitizeDisplayText(verifierStep.title)}
            </span>
            <label className="block space-y-0.5">
              <span className="text-[10px] text-[var(--text-tertiary)]">{g.verifierArgvLabel}</span>
              <textarea
                value={verifierArgv}
                onChange={(e) => setVerifierArgv(e.target.value)}
                placeholder={g.verifierArgvPlaceholder}
                rows={2}
                className="w-full rounded-[var(--radius-md)] border border-[var(--border)] bg-transparent px-2.5 py-1.5 font-mono text-[11px] text-[var(--text-primary)] outline-none focus:border-[var(--accent)]"
              />
            </label>
            <div className="grid grid-cols-3 gap-2">
              <label className="block space-y-0.5">
                <span className="text-[10px] text-[var(--text-tertiary)]">{g.verifierCwdLabel}</span>
                <input value={verifierCwd} onChange={(e) => setVerifierCwd(e.target.value)}
                  className="w-full rounded-[var(--radius-md)] border border-[var(--border)] bg-transparent px-2 py-1 text-[var(--text-primary)] outline-none focus:border-[var(--accent)]" />
              </label>
              <label className="block space-y-0.5">
                <span className="text-[10px] text-[var(--text-tertiary)]">{g.verifierTimeoutLabel}</span>
                <input type="number" value={verifierTimeout} onChange={(e) => setVerifierTimeout(e.target.value)}
                  className="w-full rounded-[var(--radius-md)] border border-[var(--border)] bg-transparent px-2 py-1 text-[var(--text-primary)] outline-none focus:border-[var(--accent)]" />
              </label>
              <label className="block space-y-0.5">
                <span className="text-[10px] text-[var(--text-tertiary)]">{g.verifierMaxOutputLabel}</span>
                <input type="number" value={verifierMaxOut} onChange={(e) => setVerifierMaxOut(e.target.value)}
                  className="w-full rounded-[var(--radius-md)] border border-[var(--border)] bg-transparent px-2 py-1 text-[var(--text-primary)] outline-none focus:border-[var(--accent)]" />
              </label>
            </div>
            <div className="flex gap-2">
              <Button size="sm" variant="primary" disabled={busy || !verifierArgv.trim()}
                onClick={() => void submitVerifier(false)}>
                {g.verifierSubmit}
              </Button>
              {verifierStep.verifier && (
                <Button size="sm" variant="secondary" disabled={busy}
                  onClick={() => void submitVerifier(true)}>
                  {g.verifierClear}
                </Button>
              )}
              <Button size="sm" variant="ghost" onClick={() => setVerifierStep(null)}>
                {g.back}
              </Button>
            </div>
          </div>
        </div>
      )}

      {/* ── report ── */}
      {report && (
        <div className="flex max-h-[45%] flex-col border-t border-[var(--border-subtle)]" data-testid="goal-report">
          <div className="flex items-center gap-2 px-4 py-2">
            <span className="flex-1 text-[12px] font-medium text-[var(--text-primary)]">
              {g.reportTitle}
            </span>
            <Button size="sm" variant={report.format === "markdown" ? "primary" : "secondary"}
              onClick={() => void loadReport("markdown")}>
              {g.reportMarkdown}
            </Button>
            <Button size="sm" variant={report.format === "json" ? "primary" : "secondary"}
              onClick={() => void loadReport("json")}>
              {g.reportJson}
            </Button>
            <Button size="sm" variant="ghost" onClick={() => setReport(null)}>
              <X className="h-3.5 w-3.5" />
            </Button>
          </div>
          <pre className="min-h-0 flex-1 overflow-auto px-4 pb-3 font-mono text-[10px] whitespace-pre-wrap text-[var(--text-secondary)]">
            {reportLoading ? g.reportLoading : sanitizeDisplayText(report.content)}
          </pre>
        </div>
      )}
    </DialogShell>
  );
}
