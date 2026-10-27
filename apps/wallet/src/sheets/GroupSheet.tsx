import { useCallback, useEffect, useState } from "react";
import { api } from "../api";
import type { GroupJoiner, GroupView } from "../api/types";
import { Icon } from "../components/Icon";
import { CopyButton, QR, Sheet, useBusy } from "../components/ui";
import { errorText } from "../format";
import { KeyPicker } from "../hw/KeyPicker";
import { useWallet } from "../state/store";
import { QuorumPicker } from "./VaultSheet";

const short = (npub: string) => `${npub.slice(0, 10)}…${npub.slice(-4)}`;

function useError() {
  const [error, setError] = useState<string | null>(null);
  const line = error && (
    <p className="field-error" role="alert" style={{ marginTop: 10 }}>
      <Icon name="warn" size={14} /> {error}
    </p>
  );
  return [setError, line] as const;
}

/** Make a group vault: name, quorum, your name. */
function CreateGroup({ onMade }: { onMade: (v: GroupView) => void }) {
  const { open } = useWallet();
  const [step, setStep] = useState<"quorum" | "name">("quorum");
  const [m, setM] = useState(2);
  const [n, setN] = useState(3);
  const [name, setName] = useState("");
  const [me, setMe] = useState("");
  const [busy, run] = useBusy();
  const [setError, errorLine] = useError();

  if (step === "quorum") {
    return (
      <Sheet
        title="Vault with others"
        left={
          <button className="text-button" onClick={() => open({ name: "vault" })}>
            Back
          </button>
        }
      >
        <p className="group-note" style={{ marginTop: 0 }}>
          Everyone joins on their own phone and adds their own key. Nobody ever sees anyone else's keys or recovery phrase.
        </p>
        <QuorumPicker m={m} n={n} onChange={(a, b) => (setM(a), setN(b))} />
        <button className="btn primary block" disabled={m < 1} onClick={() => setStep("name")}>
          Continue
        </button>
      </Sheet>
    );
  }
  return (
    <Sheet
      title="Name it"
      left={
        <button className="text-button" onClick={() => setStep("quorum")}>
          Back
        </button>
      }
    >
      <p className="group-label">Vault name</p>
      <div className="group">
        <label className="cell">
          <span className="visually-hidden">Vault name</span>
          <input value={name} onChange={(e) => setName(e.target.value)} placeholder="Family savings" maxLength={40} />
        </label>
      </div>
      <p className="group-label">Your name</p>
      <div className="group">
        <label className="cell">
          <span className="visually-hidden">Your name</span>
          <input value={me} onChange={(e) => setMe(e.target.value)} placeholder="Shown to the others" maxLength={40} />
        </label>
      </div>
      <p className="group-note">
        {m} of {n}. Next you'll get a code to invite the others.
      </p>
      <button
        className="btn primary block"
        disabled={busy || !name.trim() || !me.trim()}
        onClick={() =>
          run(async () => {
            setError(null);
            try {
              onMade(await (await api()).groupCreate({ name, threshold: m, total: n, myName: me }));
            } catch (e) {
              setError(errorText(e));
            }
          })
        }
      >
        {busy ? "Setting up…" : "Create and invite"}
      </button>
      {errorLine}
    </Sheet>
  );
}

/** Creator: the join QR and code, and requests to approve. */
function Invite({ view, onChange }: { view: GroupView; onChange: (v: GroupView) => void }) {
  const [requests, setRequests] = useState<GroupJoiner[]>([]);
  const [busy, run] = useBusy();
  const [setError, errorLine] = useError();
  const id = view.vault.groupId;

  useEffect(() => {
    let live = true;
    const poll = async () => {
      try {
        const r = await (await api()).groupRequests(id);
        if (live) setRequests(r);
      } catch {
        /* relays come and go */
      }
    };
    void poll();
    const t = setInterval(poll, 6000);
    return () => {
      live = false;
      clearInterval(t);
    };
  }, [id]);

  const code = view.code;
  return (
    <>
      {code ? (
        <>
          <p className="group-label">Invite</p>
          <QR value={code.link} />
          <div className="join-code mono" aria-label="Join code">
            {code.code}
          </div>
          <div style={{ display: "flex", gap: 10, justifyContent: "center", alignItems: "center", minHeight: 44, marginBottom: 14 }}>
            <CopyButton text={code.code} label="Copy code" />
            <CopyButton text={code.link} label="Copy link" />
          </div>
          <p className="group-note">
            In person, they scan this. Otherwise text them the code. A code only lets someone ask: you approve each person. It works for a day.
          </p>
        </>
      ) : (
        <button className="btn block" disabled={busy} onClick={() => run(async () => onChange(await (await api()).groupCode(id)))}>
          New invite code
        </button>
      )}
      {requests.length > 0 && (
        <>
          <p className="group-label">Asking to join</p>
          <div className="group">
            {requests.map((r) => (
              <div className="cell" key={r.pubkey}>
                <span className="grow">
                  <b>{r.name}</b>
                </span>
                <button
                  className="text-button"
                  disabled={busy}
                  onClick={() =>
                    run(async () => {
                      await (await api()).groupIgnore(id, r.pubkey);
                      setRequests((x) => x.filter((y) => y.pubkey !== r.pubkey));
                    })
                  }
                >
                  Ignore
                </button>
                <button
                  className="btn small primary"
                  disabled={busy}
                  onClick={() =>
                    run(async () => {
                      setError(null);
                      try {
                        onChange(await (await api()).groupApprove(id, r));
                        setRequests((x) => x.filter((y) => y.pubkey !== r.pubkey));
                      } catch (e) {
                        setError(errorText(e));
                      }
                    })
                  }
                >
                  Let in
                </button>
              </div>
            ))}
          </div>
          <p className="group-note">Only let in people you expect. Check the name with them if you're not together.</p>
        </>
      )}
      {errorLine}
    </>
  );
}

/** Creator: who holds which key. */
function AssignSlots({ view, onDone, onBack }: { view: GroupView; onDone: (v: GroupView) => void; onBack: () => void }) {
  const total = view.vault.total;
  const members = view.members;
  const [holders, setHolders] = useState<string[]>(() =>
    Array.from({ length: total }, (_, i) => (members[i] ?? members[0]).pubkey),
  );
  const [labels, setLabels] = useState<string[]>(() => Array.from({ length: total }, () => ""));
  const [busy, run] = useBusy();
  const [setError, errorLine] = useError();
  const nameOf = (p: string) => members.find((x) => x.pubkey === p)?.name ?? "Someone";
  const missing = members.filter((x) => !holders.includes(x.pubkey));

  return (
    <Sheet
      title="Who holds each key"
      left={
        <button className="text-button" onClick={onBack}>
          Back
        </button>
      }
    >
      <p className="group-note" style={{ marginTop: 0 }}>
        Each key is added by the person who holds it, on their own phone. Someone can hold more than one (say, a phone and a hardware wallet).
      </p>
      <div className="group">
        {holders.map((h, i) => (
          <div className="cell" key={i} style={{ flexWrap: "wrap", gap: 8 }}>
            <span style={{ width: 52 }}>Key {i + 1}</span>
            <select className="grow" value={h} onChange={(e) => setHolders((x) => x.map((y, j) => (j === i ? e.target.value : y)))}>
              {members.map((mm) => (
                <option key={mm.pubkey} value={mm.pubkey}>
                  {mm.pubkey === view.me ? `${mm.name} (you)` : mm.name}
                </option>
              ))}
            </select>
            <input
              value={labels[i]}
              onChange={(e) => setLabels((x) => x.map((y, j) => (j === i ? e.target.value : y)))}
              placeholder={`${nameOf(h)}'s key`}
              maxLength={32}
              style={{ flexBasis: "100%" }}
            />
          </div>
        ))}
      </div>
      {missing.length > 0 && (
        <p className="group-note">
          {missing.map((x) => x.name).join(", ")} {missing.length === 1 ? "holds" : "hold"} no key. They'll see the vault but can't sign.
        </p>
      )}
      <button
        className="btn primary block"
        disabled={busy}
        onClick={() =>
          run(async () => {
            setError(null);
            try {
              onDone(
                await (await api()).groupPropose(
                  view.vault.groupId,
                  holders.map((h, i) => ({ holder: h, label: labels[i].trim() || `${nameOf(h)}'s key` })),
                ),
              );
            } catch (e) {
              setError(errorText(e));
            }
          })
        }
      >
        Ask everyone for their keys
      </button>
      {errorLine}
    </Sheet>
  );
}

/** One group vault, from invites to a working vault. */
function GroupVaultView({ initial }: { initial: GroupView }) {
  const { close, open, refresh, notify } = useWallet();
  const [view, setView] = useState(initial);
  const [assigning, setAssigning] = useState(false);
  const [picking, setPicking] = useState<number | null>(null);
  const [busy, run] = useBusy();
  const [setError, errorLine] = useError();
  const id = view.vault.groupId;

  const reload = useCallback(async () => {
    try {
      setView(await (await api()).groupView(id));
    } catch {
      /* keep what we have */
    }
  }, [id]);

  useEffect(() => {
    const t = setInterval(reload, 6000);
    return () => clearInterval(t);
  }, [reload]);

  const s = view.setup;
  const nameOf = (p: string) => (p === view.me ? "You" : view.members.find((x) => x.pubkey === p)?.name ?? "Someone");

  if (assigning) {
    return <AssignSlots view={view} onBack={() => setAssigning(false)} onDone={(v) => (setView(v), setAssigning(false))} />;
  }

  if (picking !== null && s) {
    return (
      <KeyPicker
        index={picking}
        phoneUsed={s.slots.some((x) => x.holder === view.me && x.device === "This phone")}
        taken={s.slots.flatMap((x, i) => (i !== picking && x.fingerprint ? [x.fingerprint] : []))}
        onCancel={() => setPicking(null)}
        onPick={(k) =>
          run(async () => {
            setError(null);
            try {
              const device = k.source.type === "phone" ? "This phone" : k.source.type === "device" ? k.source.device : k.label || null;
              setView(await (await api()).groupSubmitKey(id, picking, k.key, device));
              setPicking(null);
            } catch (e) {
              setPicking(null);
              setError(errorText(e));
            }
          })
        }
      />
    );
  }

  const members = (
    <>
      <p className="group-label">People ({view.members.length})</p>
      <div className="group">
        {view.members.map((mm) => (
          <div className="cell" key={mm.pubkey}>
            <span className="grow">
              {mm.name}
              {mm.pubkey === view.me ? " (you)" : ""}
            </span>
            <span className="value mono">{short(mm.npub)}</span>
          </div>
        ))}
      </div>
    </>
  );

  const quorum = view.vault.total > 0 ? `${view.vault.threshold} of ${view.vault.total}` : s ? `${s.proposal.threshold} of ${s.slots.length}` : "";

  // Before the keys are asked for.
  if (!s) {
    const creator = view.vault.creator;
    return (
      <Sheet title={view.vault.name} left={<button className="text-button" onClick={close}>Close</button>}>
        <p className="notice">
          {creator
            ? `A ${quorum} vault. Invite everyone, then choose who holds each key.`
            : `You're in. Waiting for ${nameOf(view.members.find((x) => x.admin)?.pubkey ?? "")} to set up the keys.`}
        </p>
        {creator && <Invite view={view} onChange={setView} />}
        {members}
        {creator && (
          <button className="btn primary block" style={{ marginTop: 16 }} disabled={view.members.length < 1} onClick={() => setAssigning(true)}>
            Choose who holds each key
          </button>
        )}
        {errorLine}
        <DeleteGroup groupId={id} name={view.vault.name} creator={creator} />
      </Sheet>
    );
  }

  const allIn = s.slots.every((x) => x.key);
  const meConfirmed = s.slots.some((x) => x.holder === view.me && x.confirmed) || !!view.vault.account;
  return (
    <Sheet title={view.vault.name} left={<button className="text-button" onClick={close}>Close</button>}>
      <p className="notice">
        {s.ready
          ? "Everyone checked the vault. It's ready."
          : s.finalized
            ? meConfirmed
              ? "Waiting for the others to check the vault on their devices."
              : "Check the vault on your own device, then confirm."
            : allIn
              ? view.vault.creator
                ? "Every key is in. Finalize the vault so everyone can check it."
                : `Every key is in. Waiting for ${nameOf(s.proposer)} to finalize.`
              : view.mySlots.some((i) => !s.slots[i].key)
                ? "Add your key below."
                : "Waiting for the others to add their keys."}
      </p>

      <p className="group-label">Keys · {s.proposal.threshold} of {s.slots.length} to spend</p>
      <div className="group">
        {s.slots.map((x, i) => {
          const mine = x.holder === view.me;
          return (
            <div className="cell" key={i}>
              <span className="grow">
                <b>{x.label}</b>
                <br />
                <small className="muted">
                  {nameOf(x.holder)}
                  {x.key ? ` · ${x.device ?? "Key"} · ${x.fingerprint}` : ""}
                </small>
              </span>
              {s.finalized ? (
                x.confirmed ? <Icon name="check" size={18} /> : <small className="muted">Checking</small>
              ) : mine ? (
                <button className="btn small primary" disabled={busy} onClick={() => setPicking(i)}>
                  {x.key ? "Change" : "Add key"}
                </button>
              ) : x.key ? (
                <Icon name="check" size={18} />
              ) : (
                <small className="muted">Waiting</small>
              )}
            </div>
          );
        })}
      </div>

      {s.problems.length > 0 && (
        <div className="notice danger" role="alert">
          {s.problems.slice(-3).map((p, i) => (
            <p key={i} style={{ margin: 0 }}>
              {p}
            </p>
          ))}
        </div>
      )}

      {allIn && !s.finalized && view.vault.creator && (
        <button
          className="btn primary block"
          disabled={busy}
          onClick={() =>
            run(async () => {
              setError(null);
              try {
                setView(await (await api()).groupFinalize(id));
              } catch (e) {
                setError(errorText(e));
              }
            })
          }
        >
          Finalize vault
        </button>
      )}

      {s.finalized && !meConfirmed && (
        <>
          <p className="group-label">Check it</p>
          <div className="group">
            <div className="cell">
              <span className="grow">Vault check code</span>
              <span className="value num" style={{ fontSize: 20 }}>
                {view.shortCode}
              </span>
            </div>
            <div className="cell" style={{ display: "block" }}>
              <span>First receive address</span>
              <p className="mono" style={{ wordBreak: "break-all", margin: "6px 0 0", fontSize: 13 }}>
                {s.firstAddress}
              </p>
            </div>
          </div>
          <p className="group-note">
            Read the check code to each other: it must be the same on every phone. If you hold a hardware wallet key, register the vault on it
            after adding (Vault › Show on device) and make sure it shows this first address.
          </p>
          <button
            className="btn primary block"
            disabled={busy}
            onClick={() =>
              run(async () => {
                setError(null);
                try {
                  const a = await (await api()).groupConfirm(id);
                  await refresh();
                  notify("ok", `${a.name} added`);
                  await reload();
                } catch (e) {
                  setError(errorText(e));
                }
              })
            }
          >
            It matches · add the vault
          </button>
        </>
      )}

      {view.vault.account && (
        <button className="btn block" style={{ marginTop: 12 }} onClick={() => open({ name: "account", id: view.vault.account! })}>
          Open vault
        </button>
      )}
      {members}
      {errorLine}
      {!view.vault.account && <DeleteGroup groupId={id} name={view.vault.name} creator={view.vault.creator} />}
    </Sheet>
  );
}

/** Abandon a vault that isn't set up: everyone is taken out and it's gone from here. */
function DeleteGroup({ groupId, name, creator }: { groupId: string; name: string; creator: boolean }) {
  const { open, notify } = useWallet();
  const [busy, run] = useBusy();
  const [setError, errorLine] = useError();
  const del = () =>
    run(async () => {
      const what = creator
        ? `Delete ${name}? Everyone is taken out of it and its invite code stops working. No funds are involved: it isn't set up yet.`
        : `Leave ${name}? It's removed from this phone. No funds are involved: it isn't set up yet.`;
      if (!window.confirm(what)) return;
      setError(null);
      try {
        await (await api()).groupDelete(groupId);
        notify("ok", creator ? `Deleted ${name}` : `Left ${name}`);
        open({ name: "vault" });
      } catch (e) {
        setError(errorText(e));
      }
    });
  return (
    <>
      <button className="btn block" style={{ color: "var(--red)", marginTop: 22 }} disabled={busy} onClick={del}>
        {creator ? "Delete this vault" : "Leave this vault"}
      </button>
      {errorLine}
    </>
  );
}

/** Group vaults: create one, or open one you're in. */
export function GroupSheet({ groupId }: { groupId?: string }) {
  const [view, setView] = useState<GroupView | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!groupId) return;
    void (async () => {
      try {
        setView(await (await api()).groupView(groupId));
      } catch (e) {
        setError(errorText(e));
      }
    })();
  }, [groupId]);

  if (view) return <GroupVaultView initial={view} />;
  if (!groupId) return <CreateGroup onMade={setView} />;
  return (
    <Sheet title="Group vault">
      <p className="empty">{error ?? "Loading…"}</p>
      {error && <DeleteGroup groupId={groupId} name="this vault" creator={false} />}
    </Sheet>
  );
}
