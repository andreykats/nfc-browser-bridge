import "./style.css";
import { BridgeClient } from "../../client/src/client.js";
import { TNF_NAMES, describeRecord, matchKey, type NdefRecord } from "../../client/src/ndef.js";
import { eraseNdef, isType2, readNdef, writeNdef, type CapabilityContainer } from "../../client/src/ntag.js";
import { entryToRecord, parseApduLines, usage, type Entry } from "./builder.js";

const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;

const client = new BridgeClient();
let selected: string | null = null;
/** What we know about the card currently on each reader. */
interface CardView {
  cc?: CapabilityContainer;
  records?: NdefRecord[];
  /** Why the card could not be read (not Type 2, read error, …). */
  note?: string;
}
const views = new Map<string, CardView>();
const entries: Entry[] = [{ kind: "uri", uri: "https://topps.drey.dev/" }];

// ---- log ----

function log(msg: string, kind: "info" | "ok" | "err" = "info"): void {
  const li = document.createElement("li");
  li.className = kind;
  li.textContent = `${new Date().toLocaleTimeString()}  ${msg}`;
  $("log").prepend(li);
}

function errText(e: unknown): string {
  if (e && typeof e === "object" && "code" in e) {
    const { code, message } = e as { code: string; message?: string };
    return `${code}: ${message ?? ""}`;
  }
  return String(e);
}

// ---- connection ----

$("url").textContent = client.url;

client.on("state", (s) => {
  $("conn").dataset.state = s;
  $("conn").textContent = s;
  $("state").textContent = s;
  log(`connection ${s}`);
});

client.on("hello", (h) => {
  $("version").textContent = h.version;
  log(`hello from bridge ${h.version}`, "ok");
  for (const r of h.readers) if (r.card) void inspect(r.name, r.card.atr);
});

client.on("readers", ({ service, readers }) => {
  $("service").textContent = service;
  for (const r of readers) if (!r.card) views.delete(r.name);
  if (!selected || !readers.some((r) => r.name === selected)) {
    selected = (readers.find((r) => r.kind === "picc" && r.card) ?? readers.find((r) => r.kind === "picc") ?? readers[0])?.name ?? null;
  }
  renderReaders();
});

client.on("card_present", (e) => {
  log(`card_present ${e.reader}: uid ${e.uid ?? "?"}`, "ok");
  void inspect(e.reader, e.atr);
});

client.on("card_removed", (e) => log(`card_removed ${e.reader}`));
client.on("error", (e) => log(errText(e), "err"));

/** Read the NDEF message of the card on `reader` and show it on the reader. */
async function inspect(reader: string, atr: string): Promise<void> {
  const view: CardView = {};
  views.set(reader, view);
  if (!isType2(atr)) {
    view.note = "not an NFC Forum Type 2 card";
  } else {
    try {
      const t0 = performance.now();
      const n = await readNdef(client, reader, atr);
      view.cc = n.cc;
      view.records = n.records;
      log(`read ${n.records.length} record(s), ${n.message.length} bytes in ${Math.round(performance.now() - t0)} ms`, "ok");
    } catch (e) {
      view.note = errText(e);
      log(`read failed: ${errText(e)}`, "err");
    }
  }
  // A newer tap may have replaced this view meanwhile.
  if (views.get(reader) === view) {
    renderReaders();
    renderUsage();
  }
}

async function showPermission(): Promise<void> {
  for (const name of ["loopback-network", "local-network-access"]) {
    try {
      const status = await navigator.permissions.query({ name } as unknown as PermissionDescriptor);
      const show = () => ($("perm").textContent = `Permission "${name}": ${status.state}.`);
      show();
      status.onchange = show;
      return;
    } catch {
      // Not supported under this name.
    }
  }
}

// ---- readers ----

function renderReaders(): void {
  const box = $("readers");
  box.replaceChildren();
  if (!client.readers.length) {
    box.innerHTML = `<p class="empty">${client.state === "open" ? "No readers connected." : "Not connected to the bridge."}</p>`;
    return;
  }
  for (const r of client.readers) {
    const label = document.createElement("label");
    label.className = "reader" + (r.name === selected ? " selected" : "");
    const radio = Object.assign(document.createElement("input"), { type: "radio", name: "reader", checked: r.name === selected });
    radio.onchange = () => {
      selected = r.name;
      renderReaders();
      renderUsage();
    };
    const view = views.get(r.name);
    const cc = view?.cc;
    const info = document.createElement("div");
    info.className = "info";
    info.innerHTML = `<div class="name"></div><div class="kind"></div>${
      r.card
        ? `<div>UID <code class="uid"></code></div><div>ATR <code class="atr"></code></div><div class="cc"></div><div class="ndef"></div>`
        : `<div class="empty">no card</div>`
    }`;
    info.querySelector(".name")!.textContent = r.name;
    info.querySelector(".kind")!.textContent = r.kind;
    if (r.card) {
      info.querySelector(".uid")!.textContent = r.card.uid ?? "(none)";
      info.querySelector(".atr")!.textContent = r.card.atr;
      info.querySelector(".cc")!.textContent = cc
        ? `${cc.model}, ${cc.dataBytes} bytes (pages ${cc.firstPage}–${cc.lastPage})${cc.writable ? "" : ", read-only"}`
        : "";
      renderNdefSummary(info.querySelector(".ndef")!, view);
    }
    label.append(radio, info);
    box.append(label);
  }
}

/** The decoded NDEF message, shown on the reader right after a tap. */
function renderNdefSummary(box: Element, view: CardView | undefined): void {
  if (!view) {
    box.textContent = "reading…";
    return;
  }
  if (view.note) {
    box.textContent = view.note;
    box.classList.add("note-text");
    return;
  }
  if (!view.records) return;
  if (!view.records.length) {
    box.textContent = "NDEF: empty";
    return;
  }
  const key = matchKey(view.records);
  const title = document.createElement("div");
  title.className = "ndef-title";
  title.textContent = `NDEF: ${view.records.length} record${view.records.length === 1 ? "" : "s"}`;
  const list = document.createElement("ol");
  for (const rec of view.records) {
    const li = document.createElement("li");
    const type = new TextDecoder().decode(rec.type);
    const kind = rec.tnf === 1 ? { U: "URI", T: "Text" }[type] ?? type : `${TNF_NAMES[rec.tnf]}${type ? ` ${type}` : ""}`;
    li.innerHTML = `<span class="rtype"></span> <span class="rval"></span>`;
    li.querySelector(".rtype")!.textContent = kind;
    li.querySelector(".rval")!.textContent = describeRecord(rec);
    list.append(li);
  }
  box.append(title, list);
  if (key !== null) {
    const k = document.createElement("div");
    k.className = "matchkey";
    k.innerHTML = `Match key: <code></code>`;
    k.querySelector("code")!.textContent = key;
    box.append(k);
  }
}

function currentCard(): { reader: string; atr: string } {
  const r = client.readers.find((x) => x.name === selected);
  if (!r) throw new Error("No reader selected");
  if (!r.card) throw new Error("No card on the selected reader");
  return { reader: r.name, atr: r.card.atr };
}

/** Re-read the selected reader's card (after a write or erase). */
async function doRead(): Promise<void> {
  try {
    const { reader, atr } = currentCard();
    await inspect(reader, atr);
  } catch (e) {
    log(`read failed: ${errText(e)}`, "err");
  }
}

// ---- write ----

function renderEntries(): void {
  const box = $("entries");
  box.replaceChildren();
  entries.forEach((e, i) => {
    const row = document.createElement("div");
    row.className = "entry";
    const field = (value: string, set: (v: string) => void, placeholder = "", cls = "") => {
      const input = Object.assign(document.createElement("input"), { value, placeholder, className: cls });
      input.oninput = () => {
        set(input.value);
        renderUsage();
      };
      return input;
    };
    const tag = Object.assign(document.createElement("span"), { className: "tag", textContent: e.kind.toUpperCase() });
    row.append(tag);
    if (e.kind === "uri") row.append(field(e.uri, (v) => (e.uri = v), "https://…", "grow"));
    if (e.kind === "text") {
      row.append(field(e.text, (v) => (e.text = v), "text", "grow"), field(e.lang, (v) => (e.lang = v), "lang", "short"));
      const enc = document.createElement("select");
      enc.innerHTML = `<option value="utf-8">UTF-8</option><option value="utf-16">UTF-16</option>`;
      enc.value = e.encoding;
      enc.onchange = () => {
        e.encoding = enc.value as "utf-8" | "utf-16";
        renderUsage();
      };
      row.append(enc);
    }
    if (e.kind === "mime") {
      row.append(field(e.mimeType, (v) => (e.mimeType = v), "type/subtype", "mid"), field(e.payload, (v) => (e.payload = v), "payload", "grow"));
    }
    const rm = Object.assign(document.createElement("button"), { textContent: "✕", className: "secondary icon", title: "Remove" });
    rm.onclick = () => {
      entries.splice(i, 1);
      renderEntries();
    };
    row.append(rm);
    box.append(row);
  });
  renderUsage();
}

function buildRecords(): NdefRecord[] {
  return entries.map(entryToRecord);
}

function renderUsage(): void {
  const cc = selected ? views.get(selected)?.cc : undefined;
  const bar = $("meter-bar");
  try {
    const records = buildRecords();
    if (!cc) {
      const size = usage(records, Infinity).used;
      $("usage").textContent = `${size} bytes (place a Type 2 card to see capacity)`;
      bar.style.width = "0";
      return;
    }
    const u = usage(records, cc.dataBytes);
    $("usage").textContent = `${u.used} of ${u.capacity} bytes on ${cc.model}${u.fits ? "" : " — does not fit"}`;
    bar.style.width = `${Math.min(100, (100 * u.used) / u.capacity)}%`;
    bar.classList.toggle("over", !u.fits);
  } catch (e) {
    $("usage").textContent = errText(e);
    bar.style.width = "0";
  }
}

$("add-uri").onclick = () => (entries.push({ kind: "uri", uri: "https://" }), renderEntries());
$("add-text").onclick = () => (entries.push({ kind: "text", text: "", lang: "en", encoding: "utf-8" }), renderEntries());
$("add-mime").onclick = () => (entries.push({ kind: "mime", mimeType: "application/json", payload: "{}" }), renderEntries());

$("write").onclick = async () => {
  try {
    const { reader, atr } = currentCard();
    const records = buildRecords();
    const t0 = performance.now();
    const res = await writeNdef(client, reader, atr, records);
    log(
      `wrote ${records.length} record(s), ${res.bytesUsed} bytes, ${res.pagesWritten} page writes in ${Math.round(performance.now() - t0)} ms${
        res.usedTransparentSession ? " (transparent session fallback)" : ""
      }; verified`,
      "ok",
    );
    void doRead();
  } catch (e) {
    log(`write failed: ${errText(e)}`, "err");
  }
};

$("erase").onclick = async () => {
  try {
    const { reader, atr } = currentCard();
    await eraseNdef(client, reader, atr);
    log("erased NDEF (empty TLV)", "ok");
    void doRead();
  } catch (e) {
    log(`erase failed: ${errText(e)}`, "err");
  }
};

// ---- raw console ----

$("send").onclick = async () => {
  const out = $("apdu-out");
  try {
    if (!selected) throw new Error("No reader selected");
    const apdus = parseApduLines(($("apdus") as HTMLTextAreaElement).value);
    if (!apdus.length) throw new Error("Nothing to send");
    const res = await client.transmit(selected, apdus, { stopOnError: ($("stop") as HTMLInputElement).checked });
    out.textContent = res.map((r, i) => `> ${apdus[i]}\n< ${r.data} ${r.sw}`).join("\n");
    log(`transmit ${apdus.length} APDU(s): ${res.map((r) => r.sw).join(" ")}`, "ok");
  } catch (e) {
    out.textContent = errText(e);
    log(`transmit failed: ${errText(e)}`, "err");
  }
};

$("reset").onclick = async () => {
  try {
    if (!selected) throw new Error("No reader selected");
    const atr = await client.reset(selected);
    log(`reset: ATR ${atr}`, "ok");
  } catch (e) {
    log(`reset failed: ${errText(e)}`, "err");
  }
};

$("clear-log").onclick = () => $("log").replaceChildren();

renderEntries();
renderReaders();
void showPermission();
client.connect();
