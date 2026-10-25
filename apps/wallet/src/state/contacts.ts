import type { Contact } from "./synced";

/** name@domain (a ₿ prefix, as in BIP 353 names, is allowed and dropped). */
export function normalizeAddress(input: string): string {
  return input.trim().replace(/^₿/, "").replace(/^lightning:/i, "").toLowerCase();
}

export function isLightningAddress(input: string): boolean {
  return /^[a-z0-9._+-]+@[a-z0-9-]+(\.[a-z0-9-]+)*\.[a-z]{2,}$/.test(normalizeAddress(input));
}

export function findContact(contacts: Contact[], address: string | null | undefined): Contact | undefined {
  if (!address) return undefined;
  const a = normalizeAddress(address);
  return contacts.find((c) => normalizeAddress(c.address) === a);
}

/** Contacts matching what's typed (label or address), best first. */
export function searchContacts(contacts: Contact[], query: string): Contact[] {
  const q = query.trim().toLowerCase();
  const sorted = [...contacts].sort((a, b) => a.label.localeCompare(b.label));
  if (!q) return sorted;
  return sorted.filter((c) => c.label.toLowerCase().includes(q) || c.address.toLowerCase().includes(q));
}

/** Add or update (by address). Returns the new list, or an error message. */
export function upsertContact(contacts: Contact[], label: string, address: string, id?: string): Contact[] | string {
  const l = label.trim();
  const a = normalizeAddress(address);
  if (!l) return "Give them a name";
  if (!isLightningAddress(a)) return "That isn't a Lightning address (name@domain)";
  const clash = contacts.find((c) => normalizeAddress(c.address) === a && c.id !== id);
  if (clash) return `${clash.label} already has that address`;
  if (id) return contacts.map((c) => (c.id === id ? { ...c, label: l, address: a } : c));
  const fresh: Contact = { id: `c${Date.now().toString(36)}${Math.random().toString(36).slice(2, 6)}`, label: l, address: a, addedAt: Math.floor(Date.now() / 1000) };
  return [...contacts, fresh];
}

/** Two letters for an avatar. */
export function initials(label: string): string {
  const words = label.trim().split(/\s+/).filter(Boolean);
  const s = words.length > 1 ? words[0][0] + words[1][0] : (words[0] ?? "?").slice(0, 2);
  return s.toUpperCase();
}

/** A stable tint per contact. */
export function tint(id: string): string {
  const hues = [24, 200, 140, 280, 340, 45, 170, 220];
  let h = 0;
  for (const c of id) h = (h * 31 + c.charCodeAt(0)) >>> 0;
  return `hsl(${hues[h % hues.length]} 70% 50%)`;
}
