import { useEffect, useState } from "react";
import { api } from "../api";
import type { ChainStatus, ConnectionTest, Settings } from "../api/types";
import { errorText, formatSats } from "../format";
import { useWallet } from "../state/store";
import { Icon } from "./Icon";

/** On your own network (no need to warn about an unencrypted link). */
export function isLocalHost(url: string): boolean {
  const host = url
    .trim()
    .replace(/^\w+:\/\//, "")
    .replace(/^\[([^\]]+)\].*$/, "$1")
    .replace(/:\d+(:[st])?$/, "")
    .toLowerCase();
  if (/^(10\.|192\.168\.|127\.|169\.254\.)/.test(host)) return true;
  if (/^172\.(1[6-9]|2\d|3[01])\./.test(host)) return true;
  if (/^100\.(6[4-9]|[7-9]\d|1[01]\d|12[0-7])\./.test(host)) return true; // Tailscale
  return host === "localhost" || !host.includes(".") || /\.(local|lan|home|internal|home\.arpa)$/.test(host);
}

/** Can the wallet reach its chain server? Checked every minute while open. */
export function useChainStatus(): ChainStatus | null {
  const status = useWallet((s) => s.chain);
  return status;
}

export async function refreshChainStatus() {
  try {
    const s = await (await api()).chainStatus();
    useWallet.setState({ chain: s });
  } catch {
    /* locked, or not available: keep the last one */
  }
}

/** Home: a warning while the wallet can't reach your node. */
export function ChainWarning() {
  const status = useChainStatus();
  const open = useWallet((s) => s.open);
  if (!status || status.connected) return null;
  return (
    <p className="notice" role="alert">
      <strong>{status.custom ? `Can't reach your node (${hostOf(status.server)}).` : "Can't reach the bitcoin network."}</strong> On-chain balances may be out of date.{" "}
      <button className="text-button strong" style={{ padding: 0 }} onClick={() => open({ name: "settings" })}>
        Check connection
      </button>
    </p>
  );
}

function hostOf(url: string): string {
  return url.replace(/^\w+:\/\//, "").replace(/:\d+$/, "");
}

/** Settings › Connection: your own Electrum server, tested before it's saved. */
export function NodeConnection() {
  const notify = useWallet((s) => s.notify);
  const status = useChainStatus();
  const [settings, setSettings] = useState<Settings | null>(null);
  const [electrum, setElectrum] = useState("");
  const [esplora, setEsplora] = useState("");
  const [breezKey, setBreezKey] = useState("");
  const [more, setMore] = useState(false);
  const [test, setTest] = useState<ConnectionTest | null>(null);
  const [testing, setTesting] = useState(false);
  const [saving, setSaving] = useState(false);

  const load = async () => {
    const s = await (await api()).settings();
    setSettings(s);
    setElectrum(s.electrumUrl ?? "");
    setEsplora(s.esploraUrl ?? "");
    if (s.esploraUrl) setMore(true);
  };
  useEffect(() => {
    void load().catch(() => {});
    void refreshChainStatus();
  }, []);

  const custom = !!(settings?.electrumUrl || settings?.esploraUrl);
  const changed = electrum.trim() !== (settings?.electrumUrl ?? "") || esplora.trim() !== (settings?.esploraUrl ?? "") || !!breezKey.trim();

  const runTest = async (): Promise<ConnectionTest | null> => {
    if (!electrum.trim()) return null;
    setTesting(true);
    setTest(null);
    try {
      const t = await (await api()).testConnection(electrum.trim());
      setTest(t);
      return t;
    } catch (e) {
      const t = { ok: false, encrypted: false, millis: 0, error: errorText(e) };
      setTest(t);
      return t;
    } finally {
      setTesting(false);
    }
  };

  const save = async (opts: { electrum: string | null; esplora: string | null; force?: boolean }) => {
    let url = opts.electrum;
    if (url && !opts.force) {
      const t = await runTest();
      if (!t?.ok) return;
      url = t.url ?? url;
    }
    setSaving(true);
    try {
      await (await api()).saveSettings({ esploraUrl: opts.esplora, electrumUrl: url, hasBreezKey: false, breezApiKey: breezKey.trim() || null });
      setBreezKey("");
      await load();
      await refreshChainStatus();
      await useWallet.getState().refresh();
      notify("ok", url ? "Connected to your node" : opts.esplora ? "Saved" : "Using the default servers");
    } catch (e) {
      notify("error", errorText(e));
    } finally {
      setSaving(false);
    }
  };

  const plain = test?.ok && !test.encrypted && !isLocalHost(electrum);

  return (
    <>
      <p className="group-label">Connection</p>
      <div className="group">
        <div className="cell">
          <span
            aria-hidden
            style={{ width: 10, height: 10, borderRadius: 5, flex: "none", background: !status ? "var(--label-3)" : status.connected ? "var(--green)" : "var(--red)" }}
          />
          <span className="grow">
            {!status ? "Checking…" : status.connected ? (status.custom ? `Connected to ${hostOf(status.server)}` : "Connected to public servers") : status.custom ? `Can't reach ${hostOf(status.server)}` : "Not connected"}
            {status?.connected && status.height ? <div className="row-sub num">Block {formatSats(status.height)}</div> : null}
            {status && !status.connected && status.error ? <div className="row-sub">{status.error}</div> : null}
          </span>
          <button className="text-button" onClick={() => void refreshChainStatus()}>
            Check
          </button>
        </div>
      </div>

      <p className="group-label">Your node (Electrum server)</p>
      <div className="group">
        <label className="cell">
          <span className="visually-hidden">Electrum server address</span>
          <input
            value={electrum}
            onChange={(e) => (setElectrum(e.target.value), setTest(null))}
            placeholder="umbrel.local:50001"
            inputMode="url"
            autoCapitalize="off"
            autoCorrect="off"
            spellCheck={false}
            className="mono"
          />
        </label>
      </div>
      <p className="group-note" style={{ marginTop: -8 }}>
        Paste the address from your node's Electrum app (Umbrel: Electrs or Fulcrum › Connect), e.g. umbrel.local:50001 at home, or its IP address. Your phone or computer needs to be on the same network (or a VPN like Tailscale).
      </p>

      {test && (
        <p className={test.ok ? "group-note" : "field-error"} role="status" style={{ marginTop: -6 }}>
          <Icon name={test.ok ? "check" : "warn"} size={14} />{" "}
          {test.ok
            ? `Works: block ${formatSats(test.height ?? 0)}, ${test.millis} ms${test.encrypted ? ", encrypted" : isLocalHost(electrum) ? ", on your network" : ""}.`
            : test.error}
        </p>
      )}
      {plain && (
        <p className="field-error" role="alert">
          <Icon name="warn" size={14} /> Not encrypted: anyone between you and this server can see your addresses. Use ssl:// unless it's on a network you trust.
        </p>
      )}

      <div style={{ display: "flex", gap: 10, marginBottom: 10 }}>
        <button className="btn" style={{ flex: 1 }} disabled={testing || saving || !electrum.trim()} onClick={() => void runTest()}>
          {testing ? "Testing…" : "Test connection"}
        </button>
        <button className="btn primary" style={{ flex: 1 }} disabled={testing || saving || !changed} onClick={() => void save({ electrum: electrum.trim() || null, esplora: esplora.trim() || null })}>
          {saving ? "Connecting…" : "Save"}
        </button>
      </div>
      {test && !test.ok && electrum.trim() && (
        <p style={{ textAlign: "center", margin: "0 0 10px" }}>
          <button className="text-button" disabled={saving} onClick={() => void save({ electrum: electrum.trim(), esplora: esplora.trim() || null, force: true })}>
            Save anyway
          </button>
        </p>
      )}
      {custom && (
        <button className="btn block" style={{ marginBottom: 10 }} disabled={saving} onClick={() => (setElectrum(""), setEsplora(""), setTest(null), void save({ electrum: null, esplora: null }))}>
          Use the default servers
        </button>
      )}
      <p className="group-note" style={{ marginTop: 6 }}>
        {custom ? "Your node is used on all your linked devices, with no fallback, so nothing else sees your addresses." : "Without a node of your own, the wallet uses well-known public servers, with automatic fallback."}
      </p>

      <p style={{ textAlign: "center", margin: "4px 0 10px" }}>
        <button className="text-button subtle" aria-expanded={more} onClick={() => setMore(!more)}>
          {more ? "Fewer options" : "More options"}
        </button>
      </p>
      {more && (
        <>
          <div className="group">
            <label className="cell">
              <span className="grow">Esplora server</span>
              <input value={esplora} onChange={(e) => setEsplora(e.target.value)} placeholder="Optional" inputMode="url" autoCapitalize="off" spellCheck={false} style={{ textAlign: "right" }} />
            </label>
            <label className="cell">
              <span className="grow">Breez API key</span>
              <input
                type="password"
                value={breezKey}
                onChange={(e) => setBreezKey(e.target.value)}
                placeholder={settings?.hasBreezKey ? "Saved" : "Needed for Spending"}
                autoCapitalize="off"
                spellCheck={false}
                style={{ textAlign: "right" }}
              />
            </label>
          </div>
          <p className="group-note">An Esplora server (https://…) is used only when no Electrum server is set.</p>
        </>
      )}
      <div style={{ height: 12 }} />
    </>
  );
}
