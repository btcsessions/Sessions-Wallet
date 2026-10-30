import { useEffect, useState } from "react";
import { api } from "../api";
import type { AccountInfo, BackupStatus } from "../api/types";
import { useWallet } from "../state/store";
import type { CheckinPrefs } from "../state/synced";
import { BackupSection } from "./BackupSection";
import { Icon } from "./Icon";
import { Sheet } from "./ui";

const DAY = 86_400;
const SNOOZE_DAYS = 7;
const now = () => Math.floor(Date.now() / 1000);

export const CHECKIN_CHOICES: { days: number; label: string }[] = [
  { days: 30, label: "Monthly" },
  { days: 90, label: "Every 3 months" },
  { days: 180, label: "Every 6 months" },
  { days: 365, label: "Yearly" },
  { days: 0, label: "Off" },
];

/** The last time everything was checked: a check-in, or a backup test that passed. */
function lastChecked(c: CheckinPrefs, backup: BackupStatus | null): number {
  return Math.max(c.lastAt, backup?.lastTestedAt ?? 0);
}

export function checkinDue(c: CheckinPrefs, backup: BackupStatus | null, at = now()): boolean {
  if (!c.everyDays || !c.lastAt) return false;
  if (c.snoozedAt && at - c.snoozedAt < SNOOZE_DAYS * DAY) return false;
  return at - lastChecked(c, backup) >= c.everyDays * DAY;
}

function ago(t: number): string {
  const d = Math.floor((now() - t) / DAY);
  if (d < 1) return "today";
  if (d < 2) return "yesterday";
  if (d < 60) return `${d} days ago`;
  return `${Math.round(d / 30)} months ago`;
}

function useBackupStatus(): BackupStatus | null {
  const [s, setS] = useState<BackupStatus | null>(null);
  useEffect(() => {
    let off: (() => void) | undefined;
    let alive = true;
    void api().then(async (a) => {
      a.backupStatus()
        .then((x) => alive && setS(x))
        .catch(() => {});
      const f = await a.onBackupStatus((x) => setS(x));
      if (alive) off = f;
      else f();
    });
    return () => {
      alive = false;
      off?.();
    };
  }, []);
  return s;
}

/** Home: "Time for a backup check-in". */
export function CheckinReminder() {
  const { synced, setSynced, open } = useWallet();
  const c = synced.checkins;
  const backup = useBackupStatus();
  // A new wallet starts its clock now rather than being reminded at once.
  useEffect(() => {
    if (c.everyDays && !c.lastAt) setSynced("checkins", { ...c, lastAt: now() });
  }, [c.everyDays, c.lastAt]); // eslint-disable-line react-hooks/exhaustive-deps
  if (!checkinDue(c, backup)) return null;
  return (
    <div className="sweep-offer" role="status">
      <div className="sweep-offer-text">
        <Icon name="shield" size={18} />
        <span>
          <b>Backup check-in.</b> Your last check was {ago(lastChecked(c, backup))}. Take a minute to make sure you could still restore this wallet.
        </span>
      </div>
      <div className="sweep-offer-actions">
        <button className="text-button" onClick={() => setSynced("checkins", { ...c, snoozedAt: now() })}>
          Later
        </button>
        <button className="btn primary small" onClick={() => open({ name: "checkin" })}>
          Check now
        </button>
      </div>
    </div>
  );
}

const HOW: Record<string, string> = {
  bitbox02: "Plug it in and unlock it with your password.",
  jade: "Turn it on and unlock it with your PIN.",
  ledger: "Plug it in or turn it on, and unlock it with your PIN.",
  trezor: "Plug it in and unlock it with your PIN.",
  seedsigner: "Make sure you still have your SeedQR or seed words, and that SeedSigner boots.",
};

function DeviceRow({ account, checkedAt, onCheck }: { account: AccountInfo; checkedAt?: number; onCheck: () => void }) {
  const device = account.custody.type === "hardware" ? account.custody.device.toLowerCase() : "";
  const how = Object.entries(HOW).find(([k]) => device.includes(k))?.[1] ?? "Make sure you still have it and can unlock it.";
  return (
    <div className="cell" style={{ alignItems: "flex-start" }}>
      <span className="grow">
        <span style={{ display: "block" }}>{account.name}</span>
        <span className="contact-address" style={{ whiteSpace: "normal" }}>
          {how} {checkedAt ? `Checked ${ago(checkedAt)}.` : ""}
        </span>
      </span>
      <button className={`btn small${checkedAt && now() - checkedAt < DAY ? "" : " primary"}`} onClick={onCheck}>
        {checkedAt && now() - checkedAt < DAY ? <Icon name="check" size={16} /> : "I have it"}
      </button>
    </div>
  );
}

/** The check-in: test the backup, confirm each hardware wallet. */
export function CheckinSheet() {
  const { synced, setSynced, accounts, close, notify } = useWallet();
  const c = synced.checkins;
  const devices = accounts.filter((a) => a.custody.type === "hardware");
  const [backup, setBackup] = useState<BackupStatus | null>(null);
  const [started] = useState(now);
  const tested = !!backup?.lastTestedAt && backup.lastTestedAt >= started - 60;
  const allDevices = devices.every((d) => (c.devices[d.id] ?? 0) >= started - 60);

  const check = (id: string) => setSynced("checkins", { ...c, devices: { ...c.devices, [id]: now() } });
  const finish = () => {
    setSynced("checkins", { ...c, lastAt: now(), snoozedAt: undefined });
    notify("ok", "Check-in done. See you next time.");
    close();
  };

  return (
    <Sheet title="Backup check-in">
      <p className="group-note" style={{ marginTop: 0 }}>
        Backups only help if they work. Test yours{devices.length ? ", and make sure you still have your hardware wallets" : ""}. Nothing here moves any bitcoin.
      </p>
      <BackupSection onStatus={setBackup} />
      {devices.length > 0 && (
        <>
          <p className="group-label">Hardware wallets</p>
          <div className="group">
            {devices.map((d) => (
              <DeviceRow key={d.id} account={d} checkedAt={c.devices[d.id]} onCheck={() => check(d.id)} />
            ))}
          </div>
          <p className="group-note">Lost one? Restore its seed on a new device, then move the funds to a fresh wallet.</p>
        </>
      )}
      <button className={`btn block${tested && allDevices ? " primary" : ""}`} onClick={finish}>
        {tested && allDevices ? "All good" : "Done for now"}
      </button>
    </Sheet>
  );
}

/** Settings: how often to be reminded. */
export function CheckinSetting() {
  const { synced, setSynced, open } = useWallet();
  const c = synced.checkins;
  return (
    <>
      <p className="group-label">Backup check-ins</p>
      <div className="chips" style={{ flexWrap: "wrap" }}>
        {CHECKIN_CHOICES.map((o) => (
          <button
            key={o.days}
            className="chip"
            aria-pressed={c.everyDays === o.days}
            onClick={() => setSynced("checkins", { ...c, everyDays: o.days, lastAt: c.lastAt || now(), snoozedAt: undefined })}
          >
            {o.label}
          </button>
        ))}
      </div>
      <p className="group-note">
        {c.everyDays ? "A reminder on the home screen to test your backup and check your hardware wallets. " : "No reminders. "}
        <button className="text-button" style={{ padding: 0, fontSize: "inherit" }} onClick={() => open({ name: "checkin" })}>
          Check in now
        </button>
      </p>
    </>
  );
}
