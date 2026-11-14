// Steam Workshop discovery, subscription handoff, and installed items.
import { Compass, ExternalLink, Package, RefreshCcw, Search } from "lucide-preact";
import { useMemo, useState } from "preact/hooks";
import { api } from "../api";
import { refreshLibrary, toast, useApp } from "../store";
import { Grid, type Entry } from "./Grid";

type Mode = "discover" | "workshop";
type Sort = "trend" | "mostrecent" | "mostpopular" | "toprated";

function workshopId(value: string): string | null {
  const trimmed = value.trim();
  if (/^[0-9]+$/.test(trimmed)) return trimmed;
  try {
    const url = new URL(trimmed);
    if (!["steamcommunity.com", "www.steamcommunity.com"].includes(url.hostname)) return null;
    if (url.pathname !== "/sharedfiles/filedetails/" && url.pathname !== "/workshop/filedetails/") return null;
    const id = url.searchParams.get("id");
    return id && /^[0-9]+$/.test(id) ? id : null;
  } catch {
    return null;
  }
}

/**
 * The Steam client remains responsible for authentication, subscriptions, and
 * downloading content. Penguin Engine indexes the files after installation.
 */
export function SteamWorkshop({ mode }: { mode: Mode }) {
  const items = useApp((s) => s.items);
  const tileMin = useApp((s) => s.tileMin);
  const [query, setQuery] = useState("");
  const [sort, setSort] = useState<Sort>("trend");
  const [refreshing, setRefreshing] = useState(false);

  const installed = useMemo(
    () => items.filter((item) => item.source === "workshop"),
    [items],
  );
  const filtered = useMemo(() => {
    const words = query.toLocaleLowerCase().trim().split(/\s+/).filter(Boolean);
    return installed.filter((item) =>
      words.every((word) =>
        [item.title, item.id, ...item.tags].some((part) => part.toLocaleLowerCase().includes(word))
      ),
    );
  }, [installed, query]);
  const entries: Entry[] = useMemo(
    () => filtered.map((item) => ({ item })),
    [filtered],
  );

  async function refresh() {
    if (refreshing) return;
    setRefreshing(true);
    try {
      const added = await refreshLibrary();
      toast(added ? `Found ${added} new Workshop wallpaper${added === 1 ? "" : "s"}` : "Workshop library is up to date");
    } catch (e) {
      toast(String(e), true);
    } finally {
      setRefreshing(false);
    }
  }

  async function browse() {
    try {
      const id = workshopId(query);
      if (id) {
        await api.openWorkshop(id);
      } else {
        await api.browseWorkshop(query, sort);
      }
    } catch (e) {
      toast(String(e), true);
    }
  }

  if (mode === "discover") {
    return (
      <div class="workshop-discover">
        <div class="workshop-hero">
          <Compass size={32} color="var(--accent)" />
          <h2>Discover wallpapers</h2>
          <p>Browse Wallpaper Engine's Steam Workshop. Steam manages subscribing and downloading; Penguin Engine automatically recognizes installed items after a library refresh.</p>
          <form class="workshop-search" onSubmit={(e) => { e.preventDefault(); void browse(); }}>
            <div class="field search">
              <Search size={16} class="muted" />
              <input
                value={query}
                onInput={(e) => setQuery((e.target as HTMLInputElement).value)}
                placeholder="Search Workshop, or paste an item URL / ID"
                aria-label="Search Steam Workshop"
              />
            </div>
            <select
              class="field"
              value={sort}
              aria-label="Sort Steam Workshop results"
              onChange={(e) => setSort((e.target as HTMLSelectElement).value as Sort)}
            >
              <option value="trend">Trending</option>
              <option value="mostrecent">Most recent</option>
              <option value="mostpopular">Most popular</option>
              <option value="toprated">Top rated</option>
            </select>
            <button class="btn primary big" type="submit">
              <ExternalLink size={14} />
              Open in Steam
            </button>
          </form>
          <div class="workshop-hint">
            Sign in and choose <strong>Subscribe</strong> on Steam. When the download finishes,
            return here and switch to the Workshop tab to find and apply it.
          </div>
          <button class="btn tall" onClick={() => void refresh()} disabled={refreshing}>
            <RefreshCcw size={14} />
            {refreshing ? "Scanning…" : "Refresh installed wallpapers"}
          </button>
        </div>
      </div>
    );
  }

  return (
    <div class="browse">
      <div class="toolbar">
        <div class="field search">
          <Search size={14} class="muted" />
          <input
            value={query}
            onInput={(e) => setQuery((e.target as HTMLInputElement).value)}
            placeholder="Search installed Workshop wallpapers"
          />
        </div>
        <div class="grow" />
        <span class="result-count">{filtered.length} of {installed.length} installed</span>
        <button class="btn tall" onClick={() => void refresh()} disabled={refreshing}>
          <RefreshCcw size={14} />
          {refreshing ? "Scanning…" : "Refresh"}
        </button>
        <button class="btn primary tall" onClick={() => {
          useApp.setState({ tab: "discover" });
        }}>
          <Package size={14} />
          Find more
        </button>
      </div>
      <div class="workshop-note">
        These are Workshop items installed on disk, not necessarily every item you're subscribed to.
        New subscriptions appear after Steam downloads them.
      </div>
      <div class="grid-wrap">
        {!entries.length ? (
          <div class="coming">
            <Package size={30} />
            <div class="what">{installed.length ? "No matching Workshop items" : "No Workshop downloads found"}</div>
            <div>Subscribe through Steam, then use Refresh to scan your Steam libraries.</div>
          </div>
        ) : (
          <Grid entries={entries} tileMin={tileMin} />
        )}
      </div>
    </div>
  );
}
