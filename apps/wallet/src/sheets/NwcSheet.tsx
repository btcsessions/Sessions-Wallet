import { useEffect, useState } from "react";
import { api } from "../api";
import type { NwcBudgetPeriod, NwcConnection, NwcPermission } from "../api/types";
import { Icon } from "../components/Icon";
import { CopyButton, QR, Sheet, useBusy, useSecureScreen } from "../components/ui";
import { errorText, formatSats } from "../format";
import { useWallet } from "../state/store";

const PERIODS: { id: NwcBudgetPeriod; label: string; per: string }[] = [
  { id: "daily", label: "Day", per: "a day" },
  { id: "weekly", label: "Week", per: "a week" },
  { id: "monthly", label: "Month", per: "a month" },
  { id: "total", label: "Total", per: "in total" },
];

const BUDGETS = [10_000, 50_000, 100_000, 500_000];

const PERMISSIONS: { id: NwcPermission; label: string }[] = [
  { id: "pay", label: "Send payments (within the budget)" },
  { id: "receive", label: "Create invoices" },
  { id: "balance", label: "See your balance" },
  { id: "history", label: "See payment history" },
];

function ago(t: number | null): string {
  if (!t) return "never used";
  const s = Math.floor(Date.now() / 1000) - t;
  if (s < 90) return "used just now";
  if (s < 3600) return `used ${Math.round(s / 60)} min ago`;
  if (s < 86400) return `used ${Math.round(s / 3600)} h ago`;
  return `used ${Math.round(s / 86400)} days ago`;
}

function budgetText(c: NwcConnection): string {
  if (!c.permissions.includes("pay")) return "Can't send";
  if (c.budgetSat == null) return "No budget limit";
  const per = PERIODS.find((p) => p.id === c.budgetPeriod)?.per ?? "";
  return `${formatSats(c.spentSat)} of ${formatSats(c.budgetSat)} sats ${per}`;
}

function NewConnection({ onCreated, onCancel }: { onCreated: (uri: string, c: NwcConnection) => void; onCancel: () => void }) {
  const [name, setName] = useState("");
  const [budget, setBudget] = useState<number | null>(50_000);
  const [period, setPeriod] = useState<NwcBudgetPeriod>("monthly");
  const [permissions, setPermissions] = useState<NwcPermission[]>(["pay", "receive", "balance", "history"]);
  const [busy, run] = useBusy();
  const [error, setError] = useState<string | null>(null);

  const create = () =>
    run(async () => {
      try {
        const r = await (await api()).nwcCreate({ name: name.trim(), budgetSat: budget, budgetPeriod: period, maxPerPaymentSat: null, permissions });
        onCreated(r.uri, r.connection);
      } catch (e) {
        setError(errorText(e));
      }
    });

  return (
    <Sheet
      title="Connect an app"
      left={
        <button className="text-button" onClick={onCancel}>
          Back
        </button>
      }
    >
      <div className="group">
        <label className="cell">
          <span className="grow">App name</span>
          <input value={name} onChange={(e) => setName(e.target.value)} placeholder="e.g. Primal" maxLength={40} autoFocus style={{ textAlign: "right" }} />
        </label>
      </div>

      <p className="group-label">Spending budget</p>
      <div className="chips" style={{ flexWrap: "wrap" }}>
        {BUDGETS.map((b) => (
          <button key={b} className="chip" aria-pressed={budget === b} onClick={() => setBudget(b)}>
            {formatSats(b)}
          </button>
        ))}
        <button className="chip" aria-pressed={budget === null} onClick={() => setBudget(null)}>
          No limit
        </button>
      </div>
      {budget !== null && (
        <div className="segmented" style={{ margin: "10px 0 6px" }}>
          {PERIODS.map((p) => (
            <button key={p.id} aria-pressed={period === p.id} onClick={() => setPeriod(p.id)}>
              {p.label}
            </button>
          ))}
        </div>
      )}
      <p className="group-note" style={{ marginTop: 4 }}>
        {budget === null
          ? "The app can spend everything in Spending. Only for apps you fully trust."
          : `The app can spend up to ${formatSats(budget)} sats ${PERIODS.find((p) => p.id === period)?.per}. Payments over that are refused.`}
      </p>

      <p className="group-label">The app can</p>
      <div className="group">
        {PERMISSIONS.map((p) => (
          <label key={p.id} className="cell">
            <span className="grow">{p.label}</span>
            <input
              type="checkbox"
              checked={permissions.includes(p.id)}
              onChange={(e) => setPermissions(e.target.checked ? [...permissions, p.id] : permissions.filter((x) => x !== p.id))}
              style={{ flex: "none", width: 22, height: 22, accentColor: "var(--accent)" }}
            />
          </label>
        ))}
      </div>
      {error && <p className="field-error">{error}</p>}
      <button className="btn primary block" disabled={!name.trim() || !permissions.length || busy} onClick={create}>
        {busy ? "Creating…" : "Create connection"}
      </button>
    </Sheet>
  );
}

function ShowUri({ uri, name, onDone }: { uri: string; name: string; onDone: () => void }) {
  useSecureScreen();
  return (
    <Sheet title={`Connect ${name}`}>
      <p className="group-note" style={{ marginTop: 0 }}>
        In {name}, find Nostr Wallet Connect (often under Wallet or Zaps), then scan this code or paste the connection link.
      </p>
      <QR value={uri} />
      <div style={{ display: "flex", gap: 10, justifyContent: "center", margin: "14px 0 22px" }}>
        <CopyButton text={uri} label="Copy link" sensitive />
        <button
          className="btn small"
          onClick={async () => {
            try {
              const { openUrl } = await import("@tauri-apps/plugin-opener");
              await openUrl(uri);
            } catch {
              /* no app for it: they can scan or copy instead */
            }
          }}
        >
          <Icon name="link" size={16} /> Open app
        </button>
      </div>
      <p className="notice">This link is a key to your Spending budget. Only give it to the app you're connecting, and remove the connection here if you stop using it.</p>
      <button className="btn primary block" onClick={onDone}>
        Done
      </button>
    </Sheet>
  );
}

function ConnectionDetail({ c, onBack, onChanged }: { c: NwcConnection; onBack: () => void; onChanged: () => void }) {
  const [busy, run] = useBusy();
  const [uri, setUri] = useState<string | null>(null);
  const notify = useWallet((s) => s.notify);
  if (uri) return <ShowUri uri={uri} name={c.name} onDone={() => setUri(null)} />;
  return (
    <Sheet
      title={c.name}
      left={
        <button className="text-button" onClick={onBack}>
          Back
        </button>
      }
    >
      <div className="group">
        <div className="cell">
          <span className="grow">Budget</span>
          <span className="value">{budgetText(c)}</span>
        </div>
        <div className="cell">
          <span className="grow">Last used</span>
          <span className="value">{ago(c.lastUsedAt).replace("used ", "")}</span>
        </div>
        <label className="cell">
          <span className="grow">Active</span>
          <input
            type="checkbox"
            role="switch"
            checked={c.enabled}
            disabled={busy}
            onChange={(e) =>
              run(async () => {
                await (await api()).nwcUpdate(c.id, { enabled: e.target.checked });
                onChanged();
              })
            }
            style={{ flex: "none", width: 22, height: 22, accentColor: "var(--accent)" }}
          />
        </label>
      </div>
      <button
        className="btn block"
        style={{ marginBottom: 10 }}
        onClick={() =>
          run(async () => {
            try {
              setUri(await (await api()).nwcUri(c.id));
            } catch (e) {
              notify("error", errorText(e));
            }
          })
        }
      >
        Show connection code again
      </button>
      <button
        className="btn block"
        style={{ color: "var(--red)" }}
        disabled={busy}
        onClick={() =>
          run(async () => {
            await (await api()).nwcDelete(c.id);
            notify("ok", `${c.name} disconnected`);
            onChanged();
            onBack();
          })
        }
      >
        Disconnect
      </button>
    </Sheet>
  );
}

/** Nostr Wallet Connect: apps that can use the Spending account within a budget. */
export function NwcSheet() {
  const [list, setList] = useState<NwcConnection[] | null>(null);
  const [view, setView] = useState<{ kind: "new" } | { kind: "uri"; uri: string; name: string } | { kind: "detail"; id: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const hasSpending = useWallet((s) => s.accounts.some((a) => a.rail === "spark"));

  const load = () =>
    api()
      .then((a) => a.nwcList())
      .then(setList)
      .catch((e) => setError(errorText(e)));
  useEffect(() => {
    void load();
  }, []);

  if (view?.kind === "new")
    return (
      <NewConnection
        onCancel={() => setView(null)}
        onCreated={(uri, c) => {
          void load();
          setView({ kind: "uri", uri, name: c.name });
        }}
      />
    );
  if (view?.kind === "uri") return <ShowUri uri={view.uri} name={view.name} onDone={() => setView(null)} />;
  const detail = view?.kind === "detail" ? list?.find((c) => c.id === view.id) : undefined;
  if (detail) return <ConnectionDetail c={detail} onBack={() => setView(null)} onChanged={() => void load()} />;

  return (
    <Sheet title="Connected apps">
      <p className="group-note" style={{ marginTop: 0 }}>
        Nostr Wallet Connect lets apps like Primal, Damus or Amethyst pay from Spending (zaps, for example) within a budget you set. Turn on{" "}
        <b>Keep watching when the app is closed</b> in Settings so they work when Sessions isn't open.
      </p>
      {error && <p className="field-error">{error}</p>}
      {list && list.length > 0 && (
        <div className="group">
          {list.map((c) => (
            <button key={c.id} className="cell" onClick={() => setView({ kind: "detail", id: c.id })}>
              <span className="grow">
                <span style={{ display: "block" }}>
                  {c.name}
                  {!c.enabled && <span className="value"> · paused</span>}
                </span>
                <span className="contact-address">
                  {budgetText(c)} · {ago(c.lastUsedAt)}
                </span>
              </span>
              <Icon name="chevron" size={16} />
            </button>
          ))}
        </div>
      )}
      <button className="btn primary block" disabled={!hasSpending} onClick={() => setView({ kind: "new" })}>
        <Icon name="plus" size={16} /> Connect an app
      </button>
      {!hasSpending && <p className="group-note">Add a Spending account first.</p>}
    </Sheet>
  );
}
