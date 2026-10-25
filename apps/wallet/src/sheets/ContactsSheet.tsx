import { useState } from "react";
import { Icon } from "../components/Icon";
import { Sheet } from "../components/ui";
import { initials, searchContacts, tint, upsertContact } from "../state/contacts";
import { useWallet } from "../state/store";
import type { Contact } from "../state/synced";

export function ContactAvatar({ contact, size = 36 }: { contact: Contact; size?: number }) {
  return (
    <span className="contact-avatar" style={{ width: size, height: size, background: tint(contact.id), fontSize: size * 0.38 }} aria-hidden>
      {initials(contact.label)}
    </span>
  );
}

/** Add or edit one contact. */
export function ContactForm({ contact, address: initialAddress, onDone }: { contact?: Contact; address?: string; onDone: () => void }) {
  const { synced, setSynced, notify } = useWallet();
  const [label, setLabel] = useState(contact?.label ?? "");
  const [address, setAddress] = useState(contact?.address ?? initialAddress ?? "");
  const [error, setError] = useState<string | null>(null);

  const save = () => {
    const next = upsertContact(synced.contacts, label, address, contact?.id);
    if (typeof next === "string") return setError(next);
    setSynced("contacts", next);
    notify("ok", contact ? "Contact saved" : `${label.trim()} added to contacts`);
    onDone();
  };

  return (
    <>
      <div className="group">
        <label className="cell">
          <span className="grow">Name</span>
          <input value={label} onChange={(e) => setLabel(e.target.value)} placeholder="Who is it?" autoFocus={!contact} maxLength={40} style={{ textAlign: "right" }} />
        </label>
        <label className="cell">
          <span className="grow">Lightning address</span>
          <input
            value={address}
            onChange={(e) => setAddress(e.target.value)}
            placeholder="name@domain.com"
            inputMode="email"
            autoCapitalize="off"
            autoCorrect="off"
            spellCheck={false}
            style={{ textAlign: "right" }}
          />
        </label>
      </div>
      {error && <p className="field-error">{error}</p>}
      <button className="btn primary block" disabled={!label.trim() || !address.trim()} onClick={save} style={{ marginBottom: 10 }}>
        {contact ? "Save" : "Add contact"}
      </button>
    </>
  );
}

export function ContactsSheet({ add }: { add?: string }) {
  const { synced, setSynced, open } = useWallet();
  const [mode, setMode] = useState<{ edit?: Contact; adding?: boolean } | null>(add !== undefined ? { adding: true } : null);
  const [query, setQuery] = useState("");
  const list = searchContacts(synced.contacts, query);

  if (mode) {
    return (
      <Sheet
        title={mode.edit ? "Edit contact" : "New contact"}
        left={
          <button className="text-button" onClick={() => setMode(null)}>
            Back
          </button>
        }
      >
        <ContactForm contact={mode.edit} address={add} onDone={() => setMode(null)} />
        {mode.edit && (
          <button
            className="btn block"
            style={{ color: "var(--red)" }}
            onClick={() => {
              setSynced(
                "contacts",
                synced.contacts.filter((c) => c.id !== mode.edit!.id),
              );
              setMode(null);
            }}
          >
            Delete contact
          </button>
        )}
      </Sheet>
    );
  }

  return (
    <Sheet title="Contacts">
      {synced.contacts.length > 6 && (
        <div className="group">
          <label className="cell">
            <span className="visually-hidden">Search contacts</span>
            <input value={query} onChange={(e) => setQuery(e.target.value)} placeholder="Search" />
          </label>
        </div>
      )}
      {list.length > 0 ? (
        <div className="group">
          {list.map((c) => (
            <div key={c.id} className="cell contact-row">
              <button className="contact-main" onClick={() => open({ name: "pay", prefill: c.address })} aria-label={`Pay ${c.label}`}>
                <ContactAvatar contact={c} />
                <span className="grow">
                  <span className="contact-label">{c.label}</span>
                  <span className="contact-address">{c.address}</span>
                </span>
              </button>
              <button className="icon-button" onClick={() => setMode({ edit: c })} aria-label={`Edit ${c.label}`}>
                <Icon name="edit" size={18} />
              </button>
            </div>
          ))}
        </div>
      ) : (
        <p className="group-note" style={{ marginTop: 0 }}>
          {query ? "No one matches." : "Save the Lightning addresses you pay often. Tap a contact to pay them."}
        </p>
      )}
      <button className="btn block" onClick={() => setMode({ adding: true })}>
        <Icon name="plus" size={16} /> Add contact
      </button>
    </Sheet>
  );
}
