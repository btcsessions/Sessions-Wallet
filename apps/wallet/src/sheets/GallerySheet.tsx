import { Icon } from "../components/Icon";
import { Sheet } from "../components/ui";
import { CATALOG, SECTIONS, moduleType } from "../modules/catalog";
import { useWallet } from "../state/store";

export function GallerySheet() {
  const { layout, addModule, open, notify, setAdvanced } = useWallet();
  const installed = new Set(layout?.modules.map((m) => moduleType(m.id)));
  const advanced = layout?.advanced ?? false;

  return (
    <Sheet title="Add a module">
      {SECTIONS.map((section) => {
        const items = Object.values(CATALOG).filter((d) => d.section === section.id && (advanced || !d.advanced));
        if (!items.length) return null;
        return (
          <section className="gallery-section" key={section.id}>
            <h3>{section.title}</h3>
            <p className="lede">{section.lede}</p>
            {items.map((d) => {
              const has = installed.has(d.type);
              const canAdd = !has || d.multiple;
              return (
                <div className="gallery-item" key={d.type}>
                  <span className="gallery-icon" style={{ background: d.tint }}>
                    <Icon name={d.icon} size={24} />
                  </span>
                  <div>
                    <h4>{d.name}</h4>
                    <p>{d.blurb}</p>
                  </div>
                  {canAdd ? (
                    <button
                      className="btn small primary"
                      aria-label={`Add ${d.name}`}
                      onClick={async () => {
                        if (d.setup) {
                          open({ name: "add-account", kind: d.setup });
                          return;
                        }
                        await addModule({ id: d.type, size: d.sizes[0] });
                        notify("ok", `Added ${d.name}`);
                      }}
                    >
                      {d.setup ? "Set up" : "Add"}
                    </button>
                  ) : (
                    <span className="added">Added</span>
                  )}
                </div>
              );
            })}
          </section>
        );
      })}

      <div className="group">
        <label className="cell">
          <span className="grow">
            <div>Advanced mode</div>
            <div className="row-sub">Shows watch-only accounts and extra detail like descriptors.</div>
          </span>
          <input type="checkbox" role="switch" checked={advanced} onChange={(e) => setAdvanced(e.target.checked)} style={{ flex: "none", width: 22, height: 22, accentColor: "var(--accent)" }} />
        </label>
      </div>
    </Sheet>
  );
}
