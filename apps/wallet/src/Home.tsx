import { DndContext, KeyboardSensor, PointerSensor, TouchSensor, closestCenter, useSensor, useSensors, type DragEndEvent } from "@dnd-kit/core";
import { SortableContext, arrayMove, rectSortingStrategy, sortableKeyboardCoordinates, useSortable } from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import { useEffect, useRef } from "react";
import type { InstalledModule } from "./api/types";
import { Icon } from "./components/Icon";
import { moduleDef } from "./modules/catalog";
import { useWallet } from "./state/store";

function Tile({ module, editing }: { module: InstalledModule; editing: boolean }) {
  const { removeModule, resizeModule, setEditing } = useWallet();
  const def = moduleDef(module.id);
  const { attributes, listeners, setNodeRef, transform, transition, isDragging } = useSortable({ id: module.id, disabled: !editing });
  const press = useRef<ReturnType<typeof setTimeout> | null>(null);

  if (!def) return null;
  const Comp = def.component;

  // Long-press anywhere on a tile enters edit mode, like the iOS home screen.
  const startPress = () => {
    if (editing) return;
    press.current = setTimeout(() => {
      navigator.vibrate?.(10);
      setEditing(true);
    }, 550);
  };
  const cancelPress = () => press.current && clearTimeout(press.current);

  return (
    <div
      ref={setNodeRef}
      className={`tile ${isDragging ? "dragging" : ""}`}
      data-size={module.size}
      style={{ transform: CSS.Transform.toString(transform), transition }}
      onPointerDown={startPress}
      onPointerUp={cancelPress}
      onPointerLeave={cancelPress}
      onPointerMove={(e) => e.movementX + e.movementY > 4 && cancelPress()}
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
  const { layout, editing, setEditing, reorder, open, sync, syncing, isMock, unavailable } = useWallet();
  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 6 } }),
    useSensor(TouchSensor, { activationConstraint: { delay: 120, tolerance: 8 } }),
    useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates }),
  );

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
            <button className="icon-button" aria-label="Refresh" onClick={() => void sync()} disabled={syncing}>
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
      <h1 className="large-title">Wallet</h1>
      {unavailable.map((u) => (
        <p className="notice" key={u.id} role="status">
          <strong>{u.id === "spending" ? "Spending" : u.id} is unavailable.</strong> {u.reason}. Lock and unlock to try again.
        </p>
      ))}

      <DndContext sensors={sensors} collisionDetection={closestCenter} onDragEnd={onDragEnd}>
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
      <style>{"@keyframes spin { to { transform: rotate(360deg); } }"}</style>
    </main>
  );
}
