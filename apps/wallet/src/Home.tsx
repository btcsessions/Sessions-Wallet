import { DndContext, KeyboardSensor, MouseSensor, TouchSensor, closestCenter, useSensor, useSensors, type DragEndEvent } from "@dnd-kit/core";
import { SortableContext, arrayMove, rectSortingStrategy, sortableKeyboardCoordinates, useSortable } from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import { useEffect, useRef, useState } from "react";
import type { InstalledModule } from "./api/types";
import { Icon } from "./components/Icon";
import { Scanner, scanText } from "./components/Scanner";
import { api } from "./api";
import { moduleDef } from "./modules/catalog";
import { useWallet } from "./state/store";

function Tile({ module, editing }: { module: InstalledModule; editing: boolean }) {
  const { removeModule, resizeModule, setEditing } = useWallet();
  const def = moduleDef(module.id);
  const { attributes, listeners, setNodeRef, transform, transition, isDragging } = useSortable({ id: module.id, disabled: !editing });
  const press = useRef<{ timer: ReturnType<typeof setTimeout>; x: number; y: number } | null>(null);

  // Cancel a pending long-press as soon as the page scrolls.
  useEffect(() => {
    const onScroll = () => cancelPress();
    window.addEventListener("scroll", onScroll, { passive: true, capture: true });
    return () => window.removeEventListener("scroll", onScroll, { capture: true });
  }, []);

  if (!def) return null;
  const Comp = def.component;

  // Long-press (finger held still) enters edit mode, like the iOS home screen.
  // Any real movement, a scroll, or the browser taking the gesture cancels it.
  const startPress = (e: React.PointerEvent) => {
    if (editing || e.button > 0) return;
    cancelPress();
    press.current = {
      x: e.clientX,
      y: e.clientY,
      timer: setTimeout(() => {
        press.current = null;
        navigator.vibrate?.(12);
        setEditing(true);
      }, 500),
    };
  };
  function cancelPress() {
    if (press.current) clearTimeout(press.current.timer);
    press.current = null;
  }
  // Tapping anywhere on a card (not on one of its buttons) does what tapping
  // its main area does, e.g. opens the account.
  const tap = (e: React.MouseEvent<HTMLDivElement>) => {
    if (editing) return;
    const target = e.target as HTMLElement;
    const own = target.closest("button, a, input, select, textarea, label, [role=button], [data-no-tap]");
    if (own && own !== e.currentTarget && e.currentTarget.contains(own)) return;
    e.currentTarget.querySelector<HTMLElement>("[data-tile-open]")?.click();
  };
  const movePress = (e: React.PointerEvent) => {
    const p = press.current;
    if (p && Math.hypot(e.clientX - p.x, e.clientY - p.y) > 8) cancelPress();
  };

  return (
    <div
      ref={setNodeRef}
      className={`tile ${isDragging ? "dragging" : ""}`}
      data-size={module.size}
      style={{ transform: CSS.Transform.toString(transform), transition }}
      onPointerDown={startPress}
      onPointerUp={cancelPress}
      onPointerLeave={cancelPress}
      onPointerCancel={cancelPress}
      onPointerMove={movePress}
      onContextMenu={(e) => e.preventDefault()}
      onClick={tap}
      {...(editing ? { ...attributes, ...listeners, "aria-label": `${def.name}. Drag to reorder.` } : {})}
    >
      {editing && (
        <>
          <button className="tile-badge remove" aria-label={`Remove ${def.name}`} onPointerDown={(e) => e.stopPropagation()} onClick={() => removeModule(module.id)}>
            <Icon name="minus" size={14} stroke={3} />
          </button>
          {def.sizes.length > 1 && (
            <button className="tile-badge resize" aria-label={`Resize ${def.name}`} onPointerDown={(e) => e.stopPropagation()} onClick={() => resizeModule(module.id, def.sizes)}>
              <Icon name="resize" size={13} stroke={2.5} />
            </button>
          )}
        </>
      )}
      <Comp module={module} />
    </div>
  );
}

export function Home() {
  const { layout, editing, setEditing, reorder, open, sync, syncing, isMock, unavailable, accounts, notify } = useWallet();
  const [scanning, setScanning] = useState(false);
  const sensors = useSensors(
    // Mouse: drag as soon as it moves. Touch: hold briefly, then drag, so a
    // quick swipe always scrolls the page, even in edit mode.
    useSensor(MouseSensor, { activationConstraint: { distance: 6 } }),
    useSensor(TouchSensor, { activationConstraint: { delay: 220, tolerance: 6 } }),
    useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates }),
  );

  // Just restored: quietly look for on-chain accounts this phrase used before.
  useEffect(() => {
    if (!useWallet.getState().restored) return;
    useWallet.setState({ restored: false });
    api()
      .then((a) => a.discoverHotAccounts())
      .then((found) => {
        const used = found.filter((f) => f.txCount > 0);
        if (used.length) open({ name: "discover", found: used });
      })
      .catch(() => {});
  }, [open]);

  useEffect(() => {
    void sync();
    const t = setInterval(() => void sync(), 60_000);
    return () => clearInterval(t);
  }, [sync]);

  if (!layout) return null;
  const ids = layout.modules.map((m) => m.id);

  const onDragEnd = (e: DragEndEvent) => {
    if (!e.over || e.active.id === e.over.id) return;
    void reorder(arrayMove(ids, ids.indexOf(String(e.active.id)), ids.indexOf(String(e.over.id))));
  };

  return (
    <main className={`app ${editing ? "editing" : ""}`}>
      {isMock && <p className="mock-banner">Design preview with pretend sats. Real funds only move in the app.</p>}
      <div className="topbar">
        {editing ? (
          <button className="icon-button filled" aria-label="Add a module" onClick={() => open({ name: "gallery" })}>
            <Icon name="plus" size={18} stroke={2.5} />
          </button>
        ) : (
          <button className="icon-button" aria-label="Settings" onClick={() => open({ name: "settings" })}>
            <Icon name="gear" size={22} stroke={1.8} />
          </button>
        )}
        <div className="topbar-actions">
          {!editing && (
            <button className="icon-button" aria-label="Scan a QR code" onClick={() => setScanning(true)}>
              <Icon name="scan" size={20} stroke={1.8} />
            </button>
          )}
          {!editing && (
            <button className="icon-button" aria-label="Refresh" onClick={() => void sync(true)} disabled={syncing}>
              <span style={{ display: "grid", animation: syncing ? "spin 0.9s linear infinite" : undefined }}>
                <Icon name="refresh" size={20} stroke={1.8} />
              </span>
            </button>
          )}
          <button className={`text-button ${editing ? "strong" : ""}`} onClick={() => setEditing(!editing)}>
            {editing ? "Done" : "Edit"}
          </button>
        </div>
      </div>
      <h1 className="large-title">Sessions Wallet</h1>
      {unavailable.map((u) => (
        <p className="notice" key={u.id} role="status">
          <strong>{accounts.find((a) => a.id === u.id)?.name ?? (u.id === "spending" ? "Spending" : u.id)} is unavailable.</strong> {u.reason}, then lock and unlock to try again.
        </p>
      ))}

      <DndContext sensors={sensors} collisionDetection={closestCenter} onDragStart={() => navigator.vibrate?.(8)} onDragEnd={onDragEnd}>
        <SortableContext items={ids} strategy={rectSortingStrategy}>
          <div className="grid">
            {layout.modules.map((m) => (
              <Tile key={m.id} module={m} editing={editing} />
            ))}
            {editing && (
              <button className="add-tile" onClick={() => open({ name: "gallery" })}>
                <Icon name="plus" size={22} />
                Add a module
              </button>
            )}
          </div>
        </SortableContext>
      </DndContext>

      {!editing && layout.modules.length <= 3 && (
        <p className="subtitle" style={{ margin: "22px 0 0", textAlign: "center" }}>
          Want more? Tap Edit to add savings, a vault, hardware wallets, ecash and more.
        </p>
      )}
      {scanning && (
        <Scanner
          hint="Scan an address, invoice or ecash token to send or receive."
          onClose={() => setScanning(false)}
          onResult={(r) => {
            setScanning(false);
            scanText(r)
              .then((text) => open({ name: "pay", prefill: text }))
              .catch(() => notify("error", "That QR code isn't something this wallet can pay or receive."));
          }}
        />
      )}
      <style>{"@keyframes spin { to { transform: rotate(360deg); } }"}</style>
    </main>
  );
}
