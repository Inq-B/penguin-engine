// Wallpaper property edits are buffered so scenes aren't rebuilt for every
// slider movement. Web properties are committed more frequently; sliders
// always flush when released. Updates go to every selected monitor showing
// this same wallpaper, never to unrelated wallpapers.
import { create } from "zustand";
import { api, type Item, type Property, type Value, type Values } from "./api";

interface PropsState {
  itemId: string | null;
  output: string | null;
  /** Displays showing this wallpaper and selected by the Apply target. */
  outputs: string[];
  kind: string | null;
  props: Property[];
  values: Values;
  visible: Record<string, boolean>;
  loading: boolean;
  load: (item: Item | null, output: string | null, outputs?: string[]) => Promise<void>;
  /** commit=false while dragging or typing, commit=true on release. */
  set: (name: string, value: Value, commit?: boolean) => void;
  setMany: (values: Values) => Promise<void>;
  reset: () => Promise<void>;
  sync: (output: string, values: Values) => void;
  changed: () => Values;
}

const pending = new Map<string, Value>();
const inflight = new Set<string>();
let generation = 0;
let flushTimer: ReturnType<typeof setTimeout> | null = null;
let sending = false;

function clearFlushTimer() {
  if (flushTimer !== null) {
    clearTimeout(flushTimer);
    flushTimer = null;
  }
}

export const useProps = create<PropsState>()((set, get) => {
  const flush = async (): Promise<void> => {
    clearFlushTimer();
    if (sending || !pending.size) return;
    const { outputs } = get();
    if (!outputs.length) {
      pending.clear();
      return;
    }
    const values = Object.fromEntries(pending);
    pending.clear();
    const keys = Object.keys(values);
    const current = generation;
    sending = true;
    keys.forEach((key) => inflight.add(key));
    try {
      const replies = await Promise.allSettled(
        outputs.map((output) => api.daemon({ cmd: "set-properties", output, values })),
      );
      for (const result of replies) {
        if (result.status === "rejected") console.warn("setting wallpaper properties:", result.reason);
      }
    } finally {
      if (generation === current) {
        keys.forEach((key) => inflight.delete(key));
      }
      sending = false;
      if (pending.size) schedule(0);
    }
  };

  const schedule = (delay: number) => {
    clearFlushTimer();
    flushTimer = setTimeout(() => { void flush(); }, delay);
  };

  const evaluate = () => {
    const { props, values } = get();
    const conditions: Record<string, string> = {};
    for (const p of props) if (p.condition) conditions[p.name] = p.condition;
    if (!Object.keys(conditions).length) return;
    const current = generation;
    api.conditions(conditions, values).then((visible) => {
      if (current === generation) set({ visible });
    }).catch((e) => console.warn("property conditions:", e));
  };

  return {
    itemId: null,
    output: null,
    outputs: [],
    kind: null,
    props: [],
    values: {},
    visible: {},
    loading: false,

    load: async (item, output, outputs = output ? [output] : []) => {
      // A pending edit should reach its old wallpaper before switching away.
      if (pending.size) await flush();
      const current = ++generation;
      clearFlushTimer();
      pending.clear();
      inflight.clear();
      if (!item) {
        set({ itemId: null, output: null, outputs: [], kind: null, props: [], values: {}, visible: {}, loading: false });
        return;
      }
      set({ itemId: item.id, output, outputs, kind: item.kind, loading: true });
      try {
        const r = await api.properties(item.id, output ?? undefined);
        if (current !== generation) return;
        const values: Values = {};
        for (const p of r.properties) values[p.name] = p.value;
        set({ props: r.properties, values, visible: {}, loading: false, output: r.output });
        evaluate();
      } catch (e) {
        if (current === generation) set({ props: [], values: {}, loading: false });
        console.warn("properties:", e);
      }
    },

    set: (name, value, commit = true) => {
      set((s) => ({ values: { ...s.values, [name]: value } }));
      evaluate();
      if (!get().outputs.length) return;
      pending.set(name, value);
      // A scene rebuild can be expensive; don't repeatedly rebuild it
      // while dragging. Web pages tolerate a more responsive rate.
      schedule(commit ? 0 : get().kind === "scene" ? 450 : 100);
    },

    setMany: async (values) => {
      if (pending.size) await flush();
      set((s) => ({ values: { ...s.values, ...values } }));
      evaluate();
      const outputs = get().outputs;
      await Promise.all(outputs.map((output) => api.daemon({ cmd: "set-properties", output, values })));
    },

    reset: async () => {
      if (pending.size) await flush();
      const defaults: Values = {};
      for (const p of get().props) defaults[p.name] = p.default;
      set({ values: defaults });
      evaluate();
      await Promise.all(get().outputs.map((output) => api.daemon({ cmd: "reset-properties", output })));
    },

    sync: (output, values) => {
      if (!get().outputs.includes(output)) return;
      const next = { ...get().values };
      for (const [k, v] of Object.entries(values))
        if (!inflight.has(k) && !pending.has(k)) next[k] = v;
      set({ values: next });
      evaluate();
    },

    changed: () => {
      const { props, values } = get();
      const out: Values = {};
      for (const p of props)
        if (p.name in values && values[p.name] !== p.default) out[p.name] = values[p.name];
      return out;
    },
  };
});

/** A value dragged on a slider, sent at most one request at a time. */
export class Throttled {
  private inflight = false;
  private queued: number | null = null;
  constructor(private sendValue: (v: number) => Promise<unknown>) {}
  set(v: number) {
    if (this.inflight) {
      this.queued = v;
      return;
    }
    this.inflight = true;
    this.sendValue(v).finally(() => {
      this.inflight = false;
      if (this.queued !== null) {
        const next = this.queued;
        this.queued = null;
        this.set(next);
      }
    });
  }
}
