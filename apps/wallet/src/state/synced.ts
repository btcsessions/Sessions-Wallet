/**
 * Wallet settings that travel with the wallet (saved in it, and so in its
 * encrypted backup), not just this device: alerts, contacts, auto-sweep and
 * backup check-ins.
 */

export interface AlertPrefs {
  /** System notifications when a payment arrives (the phone's permission decides too). */
  notify: boolean;
  /** Keep the wallet connected with the app closed ("Watching for payments" service). */
  background: boolean;
}

export interface Contact {
  id: string;
  /** What you call them. */
  label: string;
  /** Lightning address (name@domain). */
  address: string;
  /** Unix seconds. */
  addedAt: number;
}

/** Move Spending's excess to cold storage once it passes a limit. */
export interface SweepRule {
  enabled: boolean;
  /** The Spending (Lightning) account it watches. */
  from: string;
  /** The on-chain account the excess goes to (hardware, savings, multisig, watch-only). */
  to: string;
  /** Offer a sweep once the balance is over this (sats). */
  thresholdSat: number;
  /** What stays in Spending after the sweep (sats). */
  keepSat: number;
  /** Unix seconds the offer was last dismissed ("Not now"): offered again once the balance grows further. */
  snoozedAtSat?: number;
}

export interface CheckinPrefs {
  /** Days between reminders; 0 = off. */
  everyDays: number;
  /** Unix seconds of the last check (a passed backup test or device check), or of turning reminders on. */
  lastAt: number;
  /** Per hardware account: unix seconds it was last checked. */
  devices: Record<string, number>;
  /** Unix seconds "Remind me later" was tapped (snoozes a week). */
  snoozedAt?: number;
}

export interface Synced {
  alerts: AlertPrefs;
  contacts: Contact[];
  sweep: SweepRule[];
  checkins: CheckinPrefs;
  /** Per hardware account: unix seconds a receive address was last confirmed on the device itself. */
  verified: Record<string, number>;
  /** Private Lightning: the ecash account payments hop through. */
  privacy: { hub: string | null };
}

export const SYNCED_DEFAULTS: Synced = {
  alerts: { notify: true, background: false },
  contacts: [],
  sweep: [],
  checkins: { everyDays: 90, lastAt: 0, devices: {} },
  verified: {},
  privacy: { hub: null },
};

export type SyncedKey = keyof Synced;
export const SYNCED_KEYS = Object.keys(SYNCED_DEFAULTS) as SyncedKey[];

/** A saved value, made safe to use (older or partial values get the defaults' missing fields). */
export function normalize<K extends SyncedKey>(key: K, v: unknown): Synced[K] {
  const d = SYNCED_DEFAULTS[key];
  if (v === undefined || v === null) return d;
  if (Array.isArray(d)) return (Array.isArray(v) ? v : d) as Synced[K];
  if (typeof v !== "object" || Array.isArray(v)) return d;
  return { ...(d as object), ...(v as object) } as Synced[K];
}
