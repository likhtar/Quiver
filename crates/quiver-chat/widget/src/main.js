// quiver-chat widget — dumb renderer. All behavior lives in the engine;
// this page only draws what the WebSocket feed sends.
// No build step: plain ES module served as-is by quiver-chat.

// Twitch retired v2 scale paths AND extensions don't exist on this CDN:
// /emoticons/v1/{id}/2.0 is the live, id-stable pattern (verified 200s).
// Static emotes: numeric ids (Kappa etc.) live on v1. Animated emotes are
// emotesv2_* ids whose animation lives on the v2 path (image/gif, probed
// live: v1/2.0 = 6KB static png, v2/dark/2.0 = 202KB animated gif).
const EMOTE_CDN = "https://static-cdn.jtvnw.net/emoticons/v1/{id}/2.0";
const EMOTE_CDN_V2 = "https://static-cdn.jtvnw.net/emoticons/v2/{id}/default/dark/2.0";

let badgeUrls = {}; // "set_id/version" -> image url
let maxMessages = 30;
let roleCss = {}; // badge set id -> css snippet for message rows
// provider tag -> {emote name -> url}; always fully populated, gating is
// done client-side via emoteFlags so disabled providers STRIP tokens.
let thirdParty = {};
let emoteFlags = { twitch: true, unicode: true, seventv: true, bttv: true, ffz: true };
// Custom badges from meta: { definitions, per_role, per_user }.
let customBadges = { definitions: {}, per_role: {}, per_user: {} };
// Lookup precedence when the same code exists in multiple providers.
const PROVIDER_ORDER = ["ffz", "bttv", "seventv"];
const EXPIRE_FADE_MS = 350;

function loadEmotes() {
  return fetch("/emotes.json")
    .then((r) => (r.ok ? r.json() : Promise.reject(r.status)))
    .then((d) => {
      thirdParty = d.providers || {};
      const n = Object.values(thirdParty).reduce((a, m) => a + Object.keys(m).length, 0);
      console.debug("[quiver] third-party emotes loaded:", n);
    });
}

loadEmotes().catch((e) => console.debug("[quiver] no third-party emotes:", e));

function lookupThirdParty(token) {
  for (const p of PROVIDER_ORDER) {
    if (!emoteFlags[p]) continue; // provider disabled
    const url = thirdParty[p]?.[token];
    if (url) return url;
  }
  return null;
}

// Does any DISABLED provider's map carry this token? Used to STRIP tokens
// that exist only under a provider the user turned off — disabling means
// the emote vanishes, never renders as literal text.
function existsOnlyInDisabledProvider(token) {
  for (const p of PROVIDER_ORDER) {
    if (emoteFlags[p]) continue; // enabled providers are lookupThirdParty's job
    if (thirdParty[p]?.[token]) return true;
  }
  return false;
}

// Unicode pictograph runs (incl. ZWJ sequences + variation selectors).
const UNICODE_EMOJI_RE = /[\p{Extended_Pictographic}\u{1F3FB}-\u{1F3FF}\uFE0F\u200D]+/gu;

function appendTokens(wrap, text) {
  if (!emoteFlags.unicode) {
    // Disabled unicode = strip emoji characters from text entirely.
    text = text.replace(UNICODE_EMOJI_RE, "");
    if (!text.trim()) return;
  }
  for (const part of text.split(/(\s+)/)) {
    if (!part) continue;
    if (/^\s+$/.test(part)) {
      wrap.append(document.createTextNode(part));
      continue;
    }
    const url = lookupThirdParty(part);
    if (url) {
      const img = document.createElement("img");
      img.className = "emote";
      img.src = url;
      img.alt = part;
      wrap.append(img);
    } else if (existsOnlyInDisabledProvider(part)) {
      // The token lives only under a provider the user disabled (7TV/BTTV/
      // FFZ): strip it entirely — same contract as twitch=false dropping
      // emote ranges and unicode=false stripping emoji characters.
    } else {
      wrap.append(document.createTextNode(part));
    }
  }
}
function el(tag, cls, text) {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text !== undefined) node.textContent = text;
  return node;
}

// Custom badges render FIRST (before Twitch CDN badges), ordered by
// priority ascending. Set = per-role (for each Twitch badge id the
// sender carries) ∪ per-user (by user id or login), deduplicated.
// Hide semantics:
//   per-role hide_native hides ONLY that role's own native badge
//   per-user hide_native hides ALL native badges for the user

function renderCustomBadges(wrap, m) {
  const chosen = new Set();
  for (const b of m.badges || []) {
    const a = customBadges.per_role?.[b.id];
    if (a) for (const id of a.badges || []) chosen.add(id);
  }
  const ua = customBadges.per_user?.[m.user_id] || customBadges.per_user?.[m.user_login];
  if (ua) for (const id of ua.badges || []) chosen.add(id);

  const ordered = [...chosen].sort((a, b) => {
    const pa = customBadges.definitions[a]?.priority ?? 0;
    const pb = customBadges.definitions[b]?.priority ?? 0;
    return pa - pb;
  });

  for (const id of ordered) {
    const d = customBadges.definitions[id];
    if (!d) continue;
    const img = document.createElement("img");
    img.className = "custom-badge";
    img.src = d.url;
    img.alt = d.label || id;
    // Height is in EM units (scales with the widget font like native
    // badges). 1 = native badge size; CSS default handles absent.
    if (d.height > 0) img.style.height = `${d.height}em`;
    if (d.label) img.title = d.label;
    wrap.append(img);
  }
}

function renderBadges(m) {
  const wrap = el("span", "badges");
  renderCustomBadges(wrap, m); // custom always present regardless

  // per-user hide_native → suppress ALL natives for this user.
  const ua = customBadges.per_user?.[m.user_id] || customBadges.per_user?.[m.user_login];
  const hideAllNative = !!ua?.hide_native;

  for (const b of m.badges || []) {
    if (hideAllNative) continue;
    // per-role hide_native → suppress exactly that role's badge, keep all
    // other native badges the user carries.
    if (customBadges.per_role?.[b.id]?.hide_native) continue;
    const url = badgeUrls[`${b.id}/${b.version}`];
    if (!url) continue; // unknown badge — skip silently
    const img = document.createElement("img");
    img.className = "badge";
    img.src = url;
    img.alt = b.id;
    wrap.append(img);
  }
  return wrap;
}

// Build the inline GIF element: mp4 → looping muted <video>, else <img>.
function gifElement(g, alt) {
  const isMp4 = /\.mp4(\?|$)/i.test(g.url);
  if (isMp4) {
    const v = document.createElement("video");
    v.className = "gif";
    v.src = g.url;
    v.muted = true;
    v.autoplay = true;
    v.loop = true;
    v.playsInline = true;
    return v;
  }
  const img = document.createElement("img");
  img.className = "gif";
  img.src = g.url;
  img.alt = alt;
  return img;
}

// Split text on emote AND gif ranges: plain slices as text nodes, ranges as
// imgs (emotes) / gif media. Ranges are UTF-16 code-unit offsets (Twitch's
// wire convention), end-exclusive — JS String.slice is UTF-16, so slicing
// here is correct by unit agreement. Do NOT "fix" this to char indices.
function renderText(m) {
  const wrap = el("span", "text");
  const ranges = [
    ...(m.emotes || []).map((e) => ({ ...e, kind: "emote" })),
    ...(m.gifs || []).map((g) => ({ ...g, kind: "gif" })),
  ].sort((a, b) => a.start - b.start);

  // Reply threads: Twitch prepends "@<parent> " to the message. We render
  // the parent above, so hide that prefix (display:none) — ranges for
  // emotes/gifs index against the ORIGINAL text, so the segment must stay
  // in place, invisible, not sliced away. Only strip when the prefix
  // matches the actual parent (never mangle a different @mention).
  let cursor = 0;
  if (m.reply_to && m.text.startsWith("@")) {
    const pre = /^@(\S+)/.exec(m.text);
    if (pre) {
      const id = pre[1].toLowerCase();
      const matchesParent =
        id === m.reply_to.user_login.toLowerCase() ||
        id === m.reply_to.display_name.toLowerCase();
      if (matchesParent) {
        let end = pre[0].length;
        if (m.text[end] === " ") end++;
        const hidden = document.createElement("span");
        hidden.className = "reply-mention-prefix";
        hidden.textContent = m.text.slice(0, end);
        wrap.append(hidden);
        cursor = end;
      }
    }
  }

  for (const r of ranges) {
    if (r.start < cursor || r.end > m.text.length) continue; // malformed/overlap
    if (r.start > cursor) appendTokens(wrap, m.text.slice(cursor, r.start));
    if (r.kind === "emote") {
      if (emoteFlags.twitch) {
        const img = document.createElement("img");
        img.className = "emote";
        const id = encodeURIComponent(r.id);
        if (r.id.startsWith("emotesv2_")) {
          // Animated variant; static v1 as fallback if v2 is unavailable.
          img.src = EMOTE_CDN_V2.replace("{id}", id);
          img.onerror = () => {
            img.onerror = null;
            img.src = EMOTE_CDN.replace("{id}", id);
          };
        } else {
          img.src = EMOTE_CDN.replace("{id}", id);
        }
        img.alt = m.text.slice(r.start, r.end);
        wrap.append(img);
      } else {
        // Twitch emotes disabled: the range is DROPPED entirely —
        // disabling a provider means its emotes Vanish from rendered
        // messages, not that the code shows as literal text (same
        // contract as unicode=false stripping emoji chars). The old
        // fall-through dropped it accidentally; this branch makes the
        // drop deliberate and equally applicable to twitch=false with
        // unicode=true.
      }
    } else {
      wrap.append(gifElement(r, m.text.slice(r.start, r.end)));
    }
    cursor = r.end;
  }
  if (cursor < m.text.length) appendTokens(wrap, m.text.slice(cursor));
  return wrap;
}

function renderMessage(m) {
  const row = el("div", "msg");
  row.dataset.id = m.id;

  // Per-role classes: one per configured badge id the sender carries.
  for (const b of m.badges || []) {
    if (roleCss[b.id]) row.classList.add(`role-${CSS.escape(b.id)}`);
  }

  // Reply thread header: compact strip above the row content.
  if (m.reply_to) {
    // Unified card: header + message share one background box.
    row.classList.add("has-reply");
    const header = el("div", "reply");
    header.append(
      el("span", "reply-arrow", "➚"),
      el("span", "reply-user", m.reply_to.display_name),
      el("span", "reply-text", m.reply_to.text),
    );
    // Click jumps to nothing (parent may be expired) but title hints.
    header.title = m.reply_to.text;
    row.append(header);
  }

  // Name + badges + separator + text share ONE wrapping container so
  // word-wrap happens INSIDE the body (beside the name) and badges can
  // never wrap away from the username — they are contiguous inline
  // content with no break point between them.
  const body = el("span", "body");
  body.append(renderBadges(m));
  const user = el("span", "user", m.display_name);
  if (m.color) user.style.color = m.color;
  body.append(user);

  if (m.is_action) {
    // /me lines: whole line italic in the sender's color, no separator.
    row.classList.add("action");
    if (m.color) row.style.color = m.color;
  } else {
    body.append(el("span", "sep", ":"));
  }
  body.append(renderText(m));
  row.append(body);
  watchHeight(row); // re-measure when media loads / role CSS resizes
  return row;
}

function trimTo(max) {
  // Count-cap is a SANITY layer: only .msg nodes count (event banners
  // are never trimmed here), newest message always kept. DOM-only —
  // backfill() would otherwise immediately put the same row back.
  while (countMsgs() > max && dropOldestRow());
}

function expire(ids) {
  for (const id of ids) {
    const node = chat.querySelector(`[data-id="${CSS.escape(id)}"]`);
    if (!node) continue;
    node.classList.add("expiring");
    setTimeout(() => {
      unwatchHeight(node);
      node.remove();
      // The row is gone and its slot free — pull the next archived message
      // back in, so expiry does not leave a hole in the window.
      settleOverflow();
    }, EXPIRE_FADE_MS);
  }
}

// A moderator/streamer deleted the message — hide it outright (no fade).
function removeMessage(id) {
  const node = chat.querySelector(`[data-id="${CSS.escape(id)}"]`);
  if (node) {
    unwatchHeight(node);
    node.remove();
    settleOverflow();
  }
}

// ---- height-based overflow management ----------------------------------
// Layered on top of the count cap: prune mode removes oldest .msg nodes
// until the container fits; scroll mode pins a scrollable chat bottom.
// Guard rails: newest message never pruned, .event banners never pruned.

let overflowMode = "prune"; // from meta.theme
const overflowWatch = new ResizeObserver(() => settleOverflow());
// Bounded archive of wire messages, so hot config reloads can re-render
// existing rows (badge maps / heights / emote flags change live — without
// this, only NEW rows would show them).
//
// It is the SOURCE OF TRUTH, not a mirror of the DOM: the rendered chat is a
// window onto it (see backfill()), so height-pruning drops rows from the DOM
// only and the archive keeps everything until the count cap evicts it.
let history = [];

function applyOverflowMode(mode) {
  overflowMode = mode === "scroll" ? "scroll" : "prune";
  chat.classList.toggle("overflow-scroll", overflowMode === "scroll");
}

function watchHeight(node) {
  overflowWatch.observe(node);
}

function unwatchHeight(node) {
  overflowWatch.unobserve(node);
}

// Re-render all history rows (e.g. after a hot config reload changed badge
// maps / heights / emote flags). Event banners are transient DOM-only
// nodes and are preserved across the redraw.
function rerender() {
  const banners = [...chat.querySelectorAll(".event")];
  for (const c of chat.children) unwatchHeight(c);
  chat.replaceChildren(...history.map(renderMessage));
  // Banners are in-flow (appended chronologically, newest LAST), so
  // re-appending in collected DOM order preserves their positions.
  for (const b of banners) chat.append(b);
  // Settle SYNCHRONOUSLY: the archive holds rows that do not fit (they are
  // pruned below), so a deferred pass would paint one frame showing messages
  // the viewer had already scrolled past.
  settleOverflow();
}

function countMsgs() {
  let n = 0;
  for (const c of chat.children) if (c.classList.contains("msg")) n++;
  return n;
}

// Does the rendered block no longer fit the viewport?
//
// scrollHeight only measures overflow on the block-END edge. Content that
// escapes past the block-START edge — which is what a container packed with
// justify-content: flex-end produces, and what any custom_css alignment
// override can bring back — is clipped and unreachable, and scrollHeight
// then stays equal to clientHeight. So a scrollHeight-only check silently
// disables pruning for those containers. Compare geometry as well: it is
// alignment-independent, so pruning works either way.
function chatOverflows() {
  if (chat.scrollHeight > chat.clientHeight + 1) return true;
  const oldest = chat.querySelector(".msg");
  if (!oldest) return false;
  const containerTop = chat.getBoundingClientRect().top;
  return oldest.getBoundingClientRect().top < containerTop - 0.5;
}

// Drop the oldest RENDERED message from the DOM, leaving `history` alone.
//
// DOM-only on purpose. `history` is the archive the visible window is
// projected from, so a pruned row stays recoverable and backfill() slides it
// back in as soon as its slot is free (an expiry, a mod-delete, or a source
// that grew). Event banners are never touched — prune the front, keep the
// newest message.
function dropOldestRow() {
  for (const child of chat.children) {
    if (!child.classList.contains("msg")) continue;
    unwatchHeight(child);
    child.remove();
    return true;
  }
  return false;
}

// Fill free space with the next-oldest archived messages, so the rendered
// window always spans as far back as the viewport allows.
//
// The window is the newest contiguous slice of `history` that fits: pruning
// takes rows off the front, and this puts the next archived row back at the
// same position when a row leaves. Bounded by maxMessages and by the layout
// itself (each candidate is measured, and dropped again if it overflows), so
// it cannot loop forever or grow past what the viewer can see.
function backfill() {
  let guard = 100; // bound the loop (safety against pathological heights)
  while (guard-- > 0) {
    if (countMsgs() >= maxMessages) return;
    const oldest = chat.querySelector(".msg");
    // Nothing rendered yet: the next message appends at the end anyway.
    if (!oldest) return;
    const oldestId = oldest.dataset.id;
    const boundary = history.findIndex((m) => m.id === oldestId);
    // Already showing the oldest archived message (or it is not in the
    // archive any more, e.g. evicted by the count cap) — nothing to add.
    if (boundary <= 0) return;
    const node = renderMessage(history[boundary - 1]);
    chat.insertBefore(node, oldest);
    if (chatOverflows()) {
      unwatchHeight(node);
      node.remove();
      return; // no room for another one either
    }
  }
}

function settleOverflow() {
  // Scroll mode: pin to bottom (only when already pinned / overflowing).
  if (overflowMode === "scroll") {
    const pinned =
      chat.scrollTop + chat.clientHeight >= chat.scrollHeight - 1 ||
      chat.scrollTop === 0;
    if (pinned && chatOverflows()) {
      chat.scrollTop = chat.scrollHeight;
    }
    return;
  }

  // Prune mode: while content overflows AND more than one .msg exists,
  // drop the oldest .msg (banners and the newest message survive).
  let guard = 100; // bound the loop (safety against pathological heights)
  while (chatOverflows() && countMsgs() > 1 && guard-- > 0) {
    if (!dropOldestRow()) break;
  }
  // Pruning leaves free space; fill it from the archive.
  backfill();
}

function applyMeta(meta) {
  if (!meta) return;
  if (meta.badges) badgeUrls = meta.badges;
  if (meta.emote_flags) emoteFlags = meta.emote_flags;
  if (meta.custom_badges) customBadges = meta.custom_badges;
  // Role map drives BOTH class assignment on new rows and the injected
  // sheet — forgetting to store it here meant styles existed but no row
  // ever matched them.
  roleCss = meta.role_css || {};
  if (meta.theme) {
    if (meta.theme.font_size_px) chat.style.fontSize = `${meta.theme.font_size_px}px`;
    if (meta.theme.max_messages) maxMessages = meta.theme.max_messages;
    if (meta.theme.overflow_mode) applyOverflowMode(meta.theme.overflow_mode);
    // Banner lifetime: seconds from config; 0 = banners stay (no auto-dismiss).
    if (meta.theme.event_banner_secs !== undefined) {
      eventBannerMs = Number(meta.theme.event_banner_secs) * 1000;
    }
  }
  applyUserStyles(meta.role_css, meta.custom_css);
}

// Injected stylesheet order: builtin < role-css < custom-css.
// At equal specificity later sheets win — so per-role snippets beat
// built-ins, and the user's global custom_css beats everything.
function applyUserStyles(roleCssMap, customCss) {
  roleCssMap = roleCssMap || {};
  let roleEl = document.getElementById("role-css");
  let customEl = document.getElementById("custom-css");

  const roleText = Object.values(roleCssMap).join("\n");
  if (!roleText) roleEl?.remove();
  else {
    if (!roleEl) {
      roleEl = document.createElement("style");
      roleEl.id = "role-css";
      document.head.append(roleEl);
    }
    roleEl.textContent = roleText;
  }

  if (!customCss) customEl?.remove();
  else {
    if (!customEl) {
      customEl = document.createElement("style");
      customEl.id = "custom-css";
      document.head.append(customEl);
    }
    customEl.textContent = customCss;
  }

  // Re-assert order — but ONLY between nodes still CONNECTED to the DOM.
  // A removed node leaves its variable truthy; append() would resurrect it
  // (this exact bug: outline survived custom_css=None).
  if (roleEl?.isConnected && customEl?.isConnected) {
    document.head.append(roleEl, customEl);
  }
}

const EVENT_STYLES = {
  sub: { icon: "★", bg: "rgba(145,70,255,.25)", border: "#9146FF" },
  gift_sub: { icon: "🎁", bg: "rgba(145,70,255,.18)", border: "#9146FF" },
  mystery_gift: { icon: "🎁🎁", bg: "rgba(145,70,255,.22)", border: "#9146FF" },
  raid: { icon: "⚔", bg: "rgba(255,140,0,.22)", border: "#FF8C00" },
  redeem: { icon: "🏅", bg: "rgba(0,180,216,.22)", border: "#00B4D8" },
  hype_train: { icon: "🚂", bg: "rgba(255,0,110,.22)", border: "#FF006E" },
  prediction: { icon: "📊", bg: "rgba(0,120,255,.18)", border: "#0078FF" },
  poll: { icon: "🗳️", bg: "rgba(0,200,120,.18)", border: "#00C878" },
  follow: { icon: "➕", bg: "rgba(0,255,180,.16)", border: "#00FFB4" },
};
let eventBannerMs = 8000;

function showEvent(ev) {
  const style = EVENT_STYLES[ev.kind];
  if (!style) return;
  // In-flow event rows live in the chat stream where they actually
  // happened. Kind-specific class (`.event-redeem`, …) is the CSS hook
  // for custom styling; inline bg/border stay as the default look.
  const banner = el("div", `event event-${ev.kind}`);
  banner.style.background = style.bg;
  banner.style.borderColor = style.border;

  let text = "";
  switch (ev.kind) {
    case "sub":
      text = ev.is_resub
        ? `${ev.display_name} resubbed (${ev.cumulative_months} mo${ev.streak_months ? `, ${ev.streak_months} streak` : ""})`
        : `${ev.display_name} subscribed!`;
      if (ev.message) banner.title = ev.message;
      break;
    case "gift_sub":
      text = `${ev.gifter_display_name || "An anonymous gifter"} gifted a sub to ${ev.recipient_display_name}`;
      break;
    case "mystery_gift":
      text = `${ev.gifter_display_name || "An anonymous gifter"} is gifting ${ev.mass_gift_count} subs!`;
      break;
    case "raid":
      text = `${ev.from_display_name} raided with ${ev.viewers} viewers!`;
      break;
    case "redeem": {
      const reward = ev.reward_title || "a channel point reward";
      text = ev.user_input
        ? `${ev.display_name} redeemed ${reward}: ${ev.user_input}`
        : `${ev.display_name} redeemed ${reward}`;
      break;
    }
    case "hype_train": {
      const top = (ev.top_display_names || []).slice(0, 3).join(", ");
      text = ev.phase === "end"
        ? `Hype Train ended at level ${ev.level}!`
        : `Hype Train level ${ev.level} — ${ev.total}/${ev.goal}${top ? ` · ${top}` : ""}`;
      break;
    }
    case "prediction":
      text = ev.phase === "end"
        ? `Prediction resolved: ${ev.title} → ${ev.winning_outcome || "?"}`
        : `${ev.phase === "lock" ? "Predictions locked" : "Prediction started"}: ${ev.title}`;
      break;
    case "poll":
      text = ev.phase === "end"
        ? `Poll ended: ${ev.title}`
        : `${ev.phase === "lock" ? "Poll locked" : "Poll started"}: ${ev.title}`;
      break;
    case "follow":
      text = `${ev.display_name} just followed!`;
      break;
  }
  // Icon: redeems (and any event carrying a reward image) show the REAL
  // Twitch reward icon — the server coalesces the channel coin for
  // default-icon rewards. The SVG coin is the last-resort fallback.
  if (ev.reward_image) {
    const icon = document.createElement("img");
    icon.className = "event-icon event-icon-img";
    icon.src = ev.reward_image;
    icon.alt = "";
    banner.append(icon);
  } else if (ev.kind === "redeem") {
    const icon = document.createElement("span");
    icon.className = "event-icon";
    // Channel-points coin: gold circle with the Twitch glitch.
    icon.innerHTML =
      '<svg viewBox="0 0 16 16" width="1.4em" height="1.4em" style="vertical-align:middle">' +
      '<circle cx="8" cy="8" r="7.2" fill="#FABE2E" stroke="#9147FF" stroke-width="1.6"/>' +
      '<path d="M11.4 5.2c-.5-.5-1.4-.9-3.4-.9-1.4 0-2.6.4-3.4.9L3.2 8l1.4 2.8c.8.5 2 .9 3.4.9s2.6-.4 3.4-.9L12.8 8l-1.4-2.8z" fill="#9147FF"/>' +
      "</svg>";
    banner.append(icon);
  } else {
    banner.append(el("span", "event-icon", style.icon));
  }
  banner.append(el("span", "event-text", text));

  // In-flow: banners appear WHERE they happened (chronological bottom of
  // the stream), not pinned above everything.
  chat.append(banner);

  // Auto-dismiss after the configured duration; `0` (theme) keeps them
  // in the flow until later messages push them out.
  if (eventBannerMs > 0) {
    setTimeout(() => {
      banner.classList.add("expiring");
      setTimeout(() => banner.remove(), EXPIRE_FADE_MS);
    }, eventBannerMs);
  }
}

function handle(wire) {
  switch (wire.type) {
    case "snapshot":
      applyMeta(wire.meta);
      history = wire.messages || [];
      for (const c of chat.children) unwatchHeight(c);
      chat.replaceChildren(...history.map(renderMessage));
      // Synchronous for the same reason as rerender(): the snapshot is the
      // whole archive, so deferring paints rows that do not fit.
      settleOverflow();
      break;
    case "message":
      // Dedupe by id: on WS join/resync there is a small race window (a
      // broadcast between the handler's drain and its snapshot read) where
      // a message is BOTH in the snapshot and delivered live. Skip if we
      // already have this exact message.
      if (history.some((m) => m.id === wire.message.id)) break;
      history.push(wire.message);
      if (history.length > maxMessages) history.shift();
      chat.append(renderMessage(wire.message));
      trimTo(maxMessages);
      requestAnimationFrame(settleOverflow);
      break;
    case "expire":
      const expired = new Set(wire.ids || []);
      history = history.filter((m) => !expired.has(m.id));
      expire(wire.ids || []);
      break;
    case "delete":
      history = history.filter((m) => m.id !== wire.id);
      removeMessage(wire.id);
      break;
    case "config":
      // Hot reload: theme/badges/emotes/CSS changed server-side. Global
      // state updates via applyMeta, then RE-RENDER existing rows so
      // badge maps, custom-badge heights, and emote flags take effect
      // live (this was the missing half — rows only updated on F5).
      applyMeta(wire.meta);
      rerender();
      // A channel swap pushes fresh badges inside meta, but the third-party
      // emote map is served separately (/emotes.json) — refetch it on every
      // config frame so a swapped channel doesn't keep rendering the old
      // channel's emotes. Re-render again once the fresh map is in.
      loadEmotes().then(rerender).catch((e) => console.debug("[quiver] emotes refetch failed:", e));
      break;
    case "event":
      showEvent(wire.event);
      break;
    case "clear":
      // Channel swapped: history wiped server-side.
      history = [];
      for (const c of chat.children) unwatchHeight(c);
      chat.replaceChildren();
      break;
    case "reload":
      // Frontend files changed on disk. Guard against rapid loops: a page
      // that just booted ignores reload frames for a moment.
      if (Date.now() - bootMs > 1500) location.reload();
      break;
  }
}

function connect() {
  const proto = location.protocol === "https:" ? "wss" : "ws";
  const ws = new WebSocket(`${proto}://${location.host}/ws`);

  ws.onmessage = (ev) => {
    try {
      handle(JSON.parse(ev.data));
    } catch (err) {
      // Surface handler failures — a silent catch here once hid a whole
      // class of "styles don't apply" bugs.
      console.error("[quiver] frame handling failed:", err);
    }
  };
  ws.onclose = () => {
    // OBS browser sources survive reconnects; retry with backoff.
    setTimeout(connect, 2000);
  };
}

const bootMs = Date.now();
const chat = document.getElementById("chat");
// Container resize (OBS changing source size/DPI) re-triggers overflow logic.
overflowWatch.observe(chat);
applyOverflowMode("prune"); // applied from meta on first snapshot
connect();
