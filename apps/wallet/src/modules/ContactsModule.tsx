import { Icon } from "../components/Icon";
import { ContactAvatar } from "../sheets/ContactsSheet";
import { searchContacts } from "../state/contacts";
import { useWallet } from "../state/store";
import type { ModuleProps } from "./catalog";

/** People you pay often: tap one to pay them. */
export function ContactsModule({ module }: ModuleProps) {
  const contacts = useWallet((s) => s.synced.contacts);
  const open = useWallet((s) => s.open);
  const max = module.size === "small" ? 3 : 7;
  const shown = searchContacts(contacts, "").slice(0, max);
  return (
    <>
      <div className="tile-title" style={{ ["--tint" as string]: "#AF52DE" }}>
        <Icon name="user" size={16} />
        Contacts
        <button data-tile-open className="visually-hidden" onClick={() => open({ name: "contacts" })}>
          All contacts
        </button>
      </div>
      {shown.length ? (
        <div className="contact-chips" style={{ margin: "4px 0 0" }}>
          {shown.map((c) => (
            <button key={c.id} className="contact-chip" onClick={() => open({ name: "pay", prefill: c.address })} aria-label={`Pay ${c.label}`}>
              <ContactAvatar contact={c} size={44} />
              <span>{c.label}</span>
            </button>
          ))}
          <button className="contact-chip add" onClick={() => open({ name: "contacts", add: "" })} aria-label="Add a contact">
            <span className="contact-avatar" style={{ width: 44, height: 44 }}>
              <Icon name="plus" size={18} />
            </span>
            <span>Add</span>
          </button>
        </div>
      ) : (
        <p className="tile-foot" style={{ marginTop: 0 }}>
          Save Lightning addresses you pay often. Tap to add one.
        </p>
      )}
    </>
  );
}
