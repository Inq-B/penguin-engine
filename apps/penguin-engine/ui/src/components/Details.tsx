// The selected wallpaper: preview, facts, actions, properties and presets.
import { open as pick } from "@tauri-apps/plugin-dialog";
import {
  ChevronDown,
  Copy,
  ExternalLink,
  FolderOpen,
  Heart,
  Layers,
  Play,
  RotateCcw,
  Search,
  Save,
  Share2,
  Trash2,
} from "lucide-preact";
import { useEffect, useMemo, useState } from "preact/hooks";
import { api, previewUrl, type Item, type Preset, type Property, type Value } from "../api";
import { humanSize, KIND_LABEL, RATING_LABEL, SOURCE_LABEL } from "../filter";
import { Throttled, useProps } from "../props";
import { apply, liveOutput, request, targetLabel, toast, toggleFavorite, useApp } from "../store";
import { MenuButton, SectionHead, Toggle } from "./ui";

export function Details() {
  const item = useApp((s) => (s.selected ? s.byId.get(s.selected) : undefined));
  if (!item) {
    return (
      <div class="details">
        <div class="hint" style={{ marginTop: 40 }}>Select a wallpaper</div>
      </div>
    );
  }
  return <ItemDetails item={item} />;
}

function ItemDetails({ item }: { item: Item }) {
  const fav = useApp((s) => !!s.favorites[item.id]);
  const res = useApp((s) => s.resolutions[item.id]);
  const status = useApp((s) => s.status);
  useApp((s) => s.target);
  const live = liveOutput(item);
  const showingOn = status?.outputs.filter((o) => o.wallpaper?.path === item.path).map((o) => o.name) ?? [];
  const meta = [KIND_LABEL[item.kind], item.size ? humanSize(item.size) : null, res ? `${res[0]} x ${res[1]}` : null]
    .filter(Boolean)
    .join(" · ");
  const ratingClass = item.rating === "mature" ? "pill bad" : item.rating === "questionable" ? "pill warn" : "pill";
  const link = item.workshop_id ? `https://steamcommunity.com/sharedfiles/filedetails/?id=${item.workshop_id}` : null;

  return (
    <div class="details">
      <div class="preview">
        {item.preview && <img src={previewUrl(item.id)} alt="" draggable={false} />}
        <div class="kind">
          <Play size={10} color="var(--accent)" />
          {KIND_LABEL[item.kind]}
        </div>
      </div>
      <h1>{item.title}</h1>
      <div class="center-row">
        <span style={{ color: "var(--accent)", fontSize: 14 }}>{SOURCE_LABEL[item.source]}</span>
        <button
          class="btn"
          style={{ width: 30, padding: 0, color: fav ? "var(--fav)" : undefined }}
          title={fav ? "Remove from favorites" : "Add to favorites"}
          onClick={() => toggleFavorite(item.id)}
        >
          <Heart size={14} fill={fav ? "currentColor" : "none"} />
        </button>
      </div>
      <div class="meta">{meta}</div>
      <div class="pills">
        {item.tags.map((t) => (
          <span class="pill">{t}</span>
        ))}
        <span class={ratingClass}>{RATING_LABEL[item.rating]}</span>
        {item.audio_reactive && <span class="pill">Audio responsive</span>}
      </div>
      {showingOn.length > 0 && <div class="hint" style={{ color: "#7bd88f" }}>Showing on {showingOn.join(", ")}</div>}
      {item.kind === "application" && <div class="hint" style={{ color: "var(--gold)" }}>Application wallpapers aren't supported.</div>}

      <div class="stack">
        <button class="btn primary tall" disabled={item.kind === "application" || !status} onClick={() => apply(item)}>
          <Play size={14} />
          Apply to {targetLabel()}
        </button>
        <div class="row">
          {link && (
            <button class="btn tall grow" onClick={() => api.openWorkshop(item.workshop_id!).catch((e) => toast(String(e), true))}>
              <ExternalLink size={14} />
              Workshop page
            </button>
          )}
          {link && (
            <button
              class="btn tall square"
              title="Copy link"
              onClick={() => navigator.clipboard.writeText(link).then(() => toast("Link copied"), () => toast("Couldn't copy the link", true))}
            >
              <Copy size={14} />
            </button>
          )}
          <button class={"btn tall" + (link ? " square" : " grow")} title="Open folder" onClick={() => api.openFolder(item.id).catch((e) => toast(String(e), true))}>
            <FolderOpen size={14} />
            {!link && "Open folder"}
          </button>
        </div>
      </div>

      <Properties item={item} live={live} />
      <Presets item={item} live={live} />
    </div>
  );
}

interface PropertySection {
  key: string;
  title: string;
  items: Property[];
}

function propertySections(props: Property[]): PropertySection[] {
  const sections: PropertySection[] = [{ key: "general", title: "General", items: [] }];
  for (const p of props) {
    if (p.type.toLowerCase() === "group") {
      sections.push({ key: p.name, title: p.label.trim() || "Other", items: [] });
    } else {
      sections[sections.length - 1].items.push(p);
    }
  }
  return sections.filter((section) => section.items.length > 0);
}

function Properties({ item, live }: { item: Item; live: string | null }) {
  const { itemId, props, values, visible, loading } = useProps();
  const out = useApp((s) => s.status?.outputs.find((o) => o.name === live));
  const [search, setSearch] = useState("");
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  const ours = itemId === item.id;
  const sound = !!out?.wallpaper?.sound;

  useEffect(() => {
    setSearch("");
    setExpanded({});
  }, [item.id]);

  const sections = useMemo(() => propertySections(props), [props]);
  const query = search.trim().toLocaleLowerCase();
  const groups = sections.map((section) => ({
    ...section,
    items: section.items.filter((p) =>
      (!p.condition || visible[p.name] !== false) &&
      (!query || [p.label, p.name, section.title].some((word) => word.toLocaleLowerCase().includes(query))),
    ),
  })).filter((section) => section.items.length > 0);

  const shown = groups.reduce((total, group) => total + group.items.length, 0);
  const total = props.filter((p) => p.type.toLowerCase() !== "group").length;

  if (!ours || (!loading && props.length === 0 && !sound)) return null;
  return (
    <>
      <SectionHead>Wallpaper settings</SectionHead>
      <div class="property-browser">
        <div class="property-toolbar">
          <label class="field property-search">
            <Search size={14} class="muted" />
            <input
              value={search}
              onInput={(e) => setSearch((e.target as HTMLInputElement).value)}
              placeholder="Find a setting…"
              aria-label="Search wallpaper settings"
            />
          </label>
          <span class="property-count">{shown}/{total}</span>
        </div>
        {!live && props.length > 0 && (
          <div class="property-message">Changes to this wallpaper will be applied when you select it.</div>
        )}
        {sound && live && <div class="property-card"><Volume output={live} volume={out?.wallpaper?.volume ?? 100} /></div>}
        {groups.map((section, index) => {
          const open = !!query || (expanded[section.key] ?? index === 0);
          const controls = section.items.filter((p) => p.type.toLowerCase() !== "text").length;
          return (
            <section class="property-section" key={section.key}>
              <button
                class="property-section-head"
                type="button"
                aria-expanded={open}
                onClick={() => setExpanded((before) => ({ ...before, [section.key]: !open }))}
              >
                <span class="property-section-title">{section.title}</span>
                <span class="property-section-count">{controls} settings</span>
                <ChevronDown size={14} class={open ? "property-chevron open" : "property-chevron"} />
              </button>
              {open && (
                <div class="property-section-body">
                  {section.items.map((p) => (
                    <Field
                      key={p.name}
                      p={p}
                      value={p.name in values ? values[p.name] : p.value}
                    />
                  ))}
                </div>
              )}
            </section>
          );
        })}
        {!loading && !groups.length && !sound && (
          <div class="property-empty">No matching settings. Try a different search.</div>
        )}
      </div>
    </>
  );
}

function Volume({ output, volume }: { output: string; volume: number }) {
  const [draft, setDraft] = useState<number | null>(null);
  const throttle = useMemo(
    () => new Throttled((v) => api.daemon({ cmd: "wallpaper-volume", output, volume: v }).catch((e) => toast(String(e), true))),
    [output],
  );
  useEffect(() => setDraft(null), [volume]);
  const v = draft ?? volume;
  return (
    <label class="prop">
      <div class="head">
        <span>Volume</span>
        <span class="value">{v}%</span>
      </div>
      <input
        type="range"
        min={0}
        max={100}
        value={v}
        onInput={(e) => {
          const n = +(e.target as HTMLInputElement).value;
          setDraft(n);
          throttle.set(n);
        }}
      />
    </label>
  );
}

function truthy(v: Value): boolean {
  return v === true || (typeof v === "number" && v !== 0) || v === "1" || (typeof v === "string" && v.toLowerCase() === "true");
}

function num(v: Value, fallback: number): number {
  const n = typeof v === "number" ? v : typeof v === "string" ? parseFloat(v) : typeof v === "boolean" ? +v : NaN;
  return Number.isFinite(n) ? n : fallback;
}

function looselyEqual(a: Value, b: Value): boolean {
  if (a === b) return true;
  const x = num(a, NaN), y = num(b, NaN);
  return Number.isFinite(x) && x === y;
}

/** `"r g b"` (0..1, or 0..255 in some wallpapers) to `#rrggbb`. */
function toHex(v: Value): string {
  const parts = String(v ?? "").trim().split(/\s+/).map(Number);
  if (parts.length < 3 || parts.some((n) => !Number.isFinite(n))) return "#ffffff";
  const scale = parts.slice(0, 3).some((n) => n > 1) ? 1 : 255;
  return "#" + parts.slice(0, 3).map((n) => Math.round(Math.min(255, Math.max(0, n * scale))).toString(16).padStart(2, "0")).join("");
}

function fromHex(hex: string): string {
  const n = parseInt(hex.slice(1), 16);
  const f = (x: number) => +(x / 255).toFixed(5);
  return `${f((n >> 16) & 255)} ${f((n >> 8) & 255)} ${f(n & 255)}`;
}

function Field({ p, value }: { p: Property; value: Value }) {
  const set = useProps((s) => s.set);
  const kind = p.type.toLowerCase();
  switch (kind) {
    case "bool":
      return (
        <div class="prop inline">
          <span>{p.label}</span>
          <Toggle on={truthy(value)} onChange={(on) => set(p.name, on)} />
        </div>
      );
    case "slider": {
      const min = p.min ?? 0;
      const max = Math.max(p.max ?? 1, min + 1e-6);
      const step = p.step && p.step > 0 ? p.step : (max - min) / 100;
      const integer = Number.isInteger(step) && Number.isInteger(min);
      const v = Math.min(max, Math.max(min, num(value, min)));
      return (
        <label class="prop">
          <div class="head">
            <span>{p.label}</span>
            <span class="value">{integer ? v.toFixed(0) : v.toFixed(2)}</span>
          </div>
          <input
            type="range"
            min={min}
            max={max}
            step={step}
            value={v}
            onInput={(e) => {
              const n = +(e.target as HTMLInputElement).value;
              set(p.name, integer ? Math.round(n) : n, false);
            }}
            onChange={(e) => {
              const n = +(e.target as HTMLInputElement).value;
              set(p.name, integer ? Math.round(n) : n, true);
            }}
          />
        </label>
      );
    }
    case "combo": {
      const options = p.options ?? [];
      const index = options.findIndex((o) => looselyEqual(o.value, value));
      return (
        <label class="prop">
          <span>{p.label}</span>
          <select class="field" style={{ height: 30 }} value={String(index)} onChange={(e) => set(p.name, options[+(e.target as HTMLSelectElement).value].value)}>
            {index < 0 && <option value="-1">{String(value)}</option>}
            {options.map((o, i) => (
              <option value={String(i)}>{o.label ?? String(o.value)}</option>
            ))}
          </select>
        </label>
      );
    }
    case "color":
      return (
        <label class="prop inline">
          <span>{p.label}</span>
          <input type="color" class="color-input" value={toHex(value)} onInput={(e) => set(p.name, fromHex((e.target as HTMLInputElement).value), false)} onChange={(e) => set(p.name, fromHex((e.target as HTMLInputElement).value), true)} />
        </label>
      );
    case "textinput":
      return (
        <label class="prop">
          <span>{p.label}</span>
          <input class="field" style={{ height: 30 }} value={String(value ?? "")} onInput={(e) => set(p.name, (e.target as HTMLInputElement).value, false)} onBlur={(e) => set(p.name, (e.target as HTMLInputElement).value, true)} />
        </label>
      );
    case "text":
      return (
        <details class="property-note">
          <summary>{p.label.length > 100 ? p.label.slice(0, 100) + "…" : p.label}</summary>
          <div class="property-note-body">{p.label}</div>
        </details>
      );
    case "group":
      return null;
    case "file":
    case "scenetexture":
    case "directory": {
      const path = typeof value === "string" ? value : "";
      const directory = kind === "directory";
      const choose = async () => {
        const chosen = await pick({ directory, multiple: false });
        if (typeof chosen === "string") set(p.name, chosen);
      };
      return (
        <div class="prop">
          <span>{p.label}</span>
          <div class="file">
            <span title={path}>{path ? path.split("/").pop() : "None"}</span>
            <button class="btn" onClick={choose}>Choose…</button>
            {path && (
              <button class="btn" onClick={() => set(p.name, "")}>
                Clear
              </button>
            )}
          </div>
        </div>
      );
    }
    default:
      return null;
  }
}

function Presets({ item, live }: { item: Item; live: string | null }) {
  const props = useProps();
  const [presets, setPresets] = useState<Preset[]>([]);
  const [naming, setNaming] = useState<string | null>(null);
  useEffect(() => {
    api.presets(item.id).then(setPresets, () => setPresets([]));
  }, [item.id]);
  if (props.itemId !== item.id || props.props.length === 0) return null;

  const save = async () => {
    const name = (naming ?? "").trim();
    if (!name) return;
    const preset: Preset = { name, wallpaper: item.id, values: props.changed() };
    try {
      await api.savePreset(preset);
      setPresets([...presets.filter((x) => x.name !== name), preset].sort((a, b) => a.name.localeCompare(b.name)));
      setNaming(null);
      toast(`Saved preset “${name}”`);
    } catch (e) {
      toast(String(e), true);
    }
  };
  const load = async (preset: Preset) => {
    const values = Object.fromEntries(props.props.map((p) => [p.name, p.name in preset.values ? preset.values[p.name] : p.default]));
    await props.setMany(values).catch((e) => toast(String(e), true));
    toast(`Loaded preset “${preset.name}”`);
  };
  const remove = async (preset: Preset) => {
    await api.deletePreset(item.id, preset.name).catch((e) => toast(String(e), true));
    setPresets(presets.filter((x) => x.name !== preset.name));
  };
  const allDisplays = async () => {
    const r = await request({ cmd: "set", wallpaper: item.path, properties: props.changed() });
    if (r) toast(`Applied “${item.title}” to all displays`);
  };
  const share = () => {
    const json = JSON.stringify({ name: item.title, wallpaper: item.id, values: props.changed() }, null, 2);
    navigator.clipboard.writeText(json).then(() => toast("Preset JSON copied"), () => toast("Couldn't copy", true));
  };

  return (
    <>
      <SectionHead>Your presets</SectionHead>
      <div class="preset-grid">
        <MenuButton
          class="btn tall"
          label={
            <>
              <FolderOpen size={14} />
              Load
            </>
          }
          menu={(close) =>
            presets.length === 0 ? (
              <div class="none">No presets saved</div>
            ) : (
              presets.map((preset) => (
                <div class="entry">
                  <button
                    onClick={() => {
                      close();
                      load(preset);
                    }}
                  >
                    {preset.name}
                  </button>
                  <button class="icon-btn" title="Delete" onClick={() => remove(preset)}>
                    <Trash2 size={13} />
                  </button>
                </div>
              ))
            )
          }
        />
        <button class="btn tall" onClick={() => setNaming(naming === null ? "" : null)}>
          <Save size={14} />
          Save
        </button>
        {naming !== null && (
          <div class="full row">
            <input
              class="field grow"
              style={{ height: 30 }}
              placeholder="Preset name"
              value={naming}
              autoFocus
              onInput={(e) => setNaming((e.target as HTMLInputElement).value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") save();
                if (e.key === "Escape") setNaming(null);
              }}
            />
            <button class="btn primary tall" disabled={!naming.trim()} onClick={save}>
              Save
            </button>
          </div>
        )}
        <button class="btn tall full" disabled={item.kind === "application"} onClick={allDisplays}>
          <Layers size={14} />
          Apply to all displays
        </button>
        <button class="btn tall full" onClick={share}>
          <Share2 size={14} />
          Share JSON
        </button>
        <button class="btn danger tall full" onClick={() => props.reset().catch((e) => toast(String(e), true))}>
          <RotateCcw size={14} />
          Reset{live ? "" : " to defaults"}
        </button>
      </div>
    </>
  );
}
