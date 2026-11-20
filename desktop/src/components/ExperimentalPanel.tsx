import { FlaskConical } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import {
  commands,
  type ExperimentalSettingsDto,
} from "../bindings";
import { getMessages } from "../lib/i18n";
import { useAppStore } from "../store/useAppStore";
import { Pill } from "./ui/Pill";

export function ExperimentalPanel() {
  const locale = useAppStore((s) => s.locale);
  const t = getMessages(locale);
  const [settings, setSettings] = useState<ExperimentalSettingsDto | null>(null);
  const [statusMessage, setStatusMessage] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    const res = await commands.getExperimentalSettings();
    if (res.status === "ok") setSettings(res.data);
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const toggleFlag = async (flag: "js_repl" | "subagents") => {
    if (!settings || busy) return;
    setBusy(true);
    setStatusMessage(null);
    const res =
      flag === "js_repl"
        ? await commands.setJsReplEnabled(!settings.js_repl_enabled)
        : await commands.setSubagentsEnabled(!settings.subagents_enabled);
    if (res.status === "ok") {
      setSettings(res.data);
    } else {
      setStatusMessage(`${res.error.code}: ${res.error.message}`);
    }
    setBusy(false);
  };

  const rows: {
    key: "js_repl" | "subagents";
    title: string;
    desc: string;
    enabled: boolean;
  }[] = [
    {
      key: "js_repl",
      title: t.experimentalPanel.jsRepl,
      desc: t.experimentalPanel.jsReplDesc,
      enabled: settings?.js_repl_enabled ?? false,
    },
    {
      key: "subagents",
      title: t.experimentalPanel.subagents,
      desc: t.experimentalPanel.subagentsDesc,
      enabled: settings?.subagents_enabled ?? false,
    },
  ];
  const anyEnabled = rows.some((row) => row.enabled);

  return (
    <div className="space-y-4 text-xs" data-testid="experimental-panel">
      {statusMessage && (
        <div className="rounded-[var(--radius-md)] border border-[var(--danger)] bg-[var(--danger-surface)] px-2.5 py-1.5 text-[11px] text-[var(--danger)]">
          {statusMessage}
        </div>
      )}
      <section className="space-y-2">
        <div className="flex items-center gap-1.5 font-medium text-[var(--text-primary)]">
          <FlaskConical className="h-3.5 w-3.5 text-[var(--accent-text)]" strokeWidth={1.75} />
          <span>{t.experimentalPanel.header}</span>
        </div>
        <p className="text-[11px] text-[var(--text-tertiary)]">
          {t.experimentalPanel.subtitle}
        </p>
        {rows.map((row) => (
          <div
            key={row.key}
            className="flex items-center justify-between gap-3 rounded-[var(--radius-md)] border border-[var(--border)] px-2.5 py-2"
          >
            <div className="min-w-0 space-y-0.5">
              <div className="flex items-center gap-2 text-[var(--text-primary)]">
                <span>{row.title}</span>
                {settings && (
                  <Pill tone={row.enabled ? "success" : "neutral"}>
                    {row.enabled ? t.experimentalPanel.enabled : t.experimentalPanel.disabled}
                  </Pill>
                )}
              </div>
              <p className="text-[11px] text-[var(--text-tertiary)]">{row.desc}</p>
            </div>
            <button
              type="button"
              role="switch"
              data-testid={`experimental-toggle-${row.key}`}
              aria-checked={row.enabled}
              aria-label={row.title}
              disabled={!settings || busy}
              onClick={() => void toggleFlag(row.key)}
              className={`relative h-5 w-9 shrink-0 rounded-full transition-colors ${
                row.enabled ? "bg-[var(--accent)]" : "bg-[var(--border)]"
              } ${!settings || busy ? "opacity-50" : ""}`}
            >
              <span
                className={`absolute top-0.5 left-0 h-4 w-4 rounded-full bg-white shadow-xs transition-transform ${
                  row.enabled ? "translate-x-[18px]" : "translate-x-0.5"
                }`}
              />
            </button>
          </div>
        ))}
        {anyEnabled && (
          <p className="text-[11px] text-[var(--text-tertiary)]">
            {t.experimentalPanel.appliesNewSessions}
          </p>
        )}
      </section>
    </div>
  );
}
