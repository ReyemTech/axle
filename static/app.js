// axle web UI: CSRF header for htmx requests, j/k selection, "/" search, filter autosubmit,
// confirmations on destructive forms, and lease bars that count down between renders.
(function () {
  "use strict";

  function csrfToken() {
    var meta = document.querySelector('meta[name="csrf-token"]');
    return meta ? meta.getAttribute("content") : null;
  }

  // Every htmx request carries the session's CSRF token (required on POSTs).
  document.addEventListener("htmx:configRequest", function (e) {
    var token = csrfToken();
    if (token) e.detail.headers["X-CSRF-Token"] = token;
  });

  var selectedId = null;

  function navItems() {
    return Array.prototype.filter.call(
      document.querySelectorAll("[data-nav]"),
      function (el) { return !el.hidden; }
    );
  }

  function select(el) {
    document.querySelectorAll("[data-nav].selected").forEach(function (x) {
      x.classList.remove("selected");
    });
    if (!el) return;
    el.classList.add("selected");
    selectedId = el.id;
    el.focus({ preventScroll: true });
    el.scrollIntoView({ block: "nearest", inline: "nearest" });
  }

  function isTyping(t) {
    return t instanceof HTMLElement &&
      (t.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName));
  }

  document.addEventListener("keydown", function (e) {
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    var typing = isTyping(e.target);
    var search = document.getElementById("search");
    if (e.key === "/" && !typing && search) {
      e.preventDefault();
      search.focus();
      search.select();
      return;
    }
    if (e.key === "Escape" && e.target === search) {
      search.blur();
      return;
    }
    if (typing) return;
    if (e.key === "j" || e.key === "k") {
      var items = navItems();
      if (!items.length) return;
      e.preventDefault();
      var i = items.findIndex(function (x) { return x.classList.contains("selected"); });
      if (i < 0) i = 0;
      else i = Math.max(0, Math.min(items.length - 1, i + (e.key === "j" ? 1 : -1)));
      select(items[i]);
    } else if (e.key === "Enter") {
      var current = document.querySelector("[data-nav].selected");
      var expand = current && current.querySelector("[data-expand]");
      if (expand && e.target === current) {
        e.preventDefault();
        expand.click();
      }
    }
  });

  // Search narrows the selectable items (cards, agents) by their text.
  function applySearch() {
    var search = document.getElementById("search");
    var q = search ? search.value.trim().toLowerCase() : "";
    document.querySelectorAll("[data-nav]").forEach(function (el) {
      el.hidden = q !== "" && el.textContent.toLowerCase().indexOf(q) < 0;
    });
  }
  document.addEventListener("input", function (e) {
    if (e.target && e.target.id === "search") applySearch();
  });

  // Cards whose subtask list is open; a live swap re-renders a card closed, so reopen it
  // (which also reloads the children).
  var openCards = new Set();
  document.addEventListener("toggle", function (e) {
    var d = e.target;
    if (!(d instanceof HTMLDetailsElement) || !d.classList.contains("children")) return;
    var card = d.closest(".card");
    if (!card || !card.id) return;
    if (d.open) openCards.add(card.id);
    else openCards.delete(card.id);
  }, true);

  function restoreOpen() {
    openCards.forEach(function (id) {
      var d = document.querySelector("#" + CSS.escape(id) + " > details.children");
      if (d && !d.open) d.open = true;
    });
  }

  // A lease element carries the seconds left when it was rendered (`data-lease-left`) out of
  // the lease's length (`data-lease-ttl`). Counting from when this page first saw the element
  // keeps it independent of the browser's clock. `--lease` (0..1) sizes the occupancy bar.
  var seenAt = new WeakMap();

  function leaseText(left) {
    var s = Math.ceil(left);
    if (s <= 0) return "expired";
    if (s < 60) return s + "s left";
    if (s < 3600) return Math.floor(s / 60) + "m left";
    return Math.floor(s / 3600) + "h " + Math.floor((s % 3600) / 60) + "m left";
  }

  function tickLeases() {
    var now = performance.now();
    document.querySelectorAll("[data-lease-left]").forEach(function (el) {
      if (!seenAt.has(el)) seenAt.set(el, now);
      var ttl = Number(el.getAttribute("data-lease-ttl")) || 1;
      var left = Number(el.getAttribute("data-lease-left")) - (now - seenAt.get(el)) / 1000;
      left = Math.max(0, Math.min(ttl, left));
      el.style.setProperty("--lease", (left / ttl).toFixed(4));
      var state = left <= 0 ? "expired" : left * 5 < ttl ? "low" : "ok";
      if (el.getAttribute("data-lease-state") !== state) el.setAttribute("data-lease-state", state);
      var label = el.querySelector(".lease-left");
      var text = leaseText(left);
      if (label && label.textContent !== text) label.textContent = text;
    });
  }
  tickLeases();
  setInterval(tickLeases, 1000);
  document.addEventListener("htmx:load", tickLeases);

  // Live swaps replace elements: keep the filter, the selection and open subtask lists.
  function afterSwap() {
    applySearch();
    restoreOpen();
    tickLeases();
    if (selectedId) {
      var el = document.getElementById(selectedId);
      if (el) el.classList.add("selected");
    }
  }
  // A live refresh of a region marked data-keep-drafts (Needs you, the task page's live
  // section with its plan form) must not wipe something being written: while it has focus,
  // an open disclosure or a typed but unsent field, the refresh waits, and runs once the
  // draft is gone (sent, cleared or abandoned with focus elsewhere).
  function hasDraft(region) {
    if (region.contains(document.activeElement) && document.activeElement !== region) {
      return true;
    }
    if (region.querySelector("details[open]")) return true;
    return Array.prototype.some.call(
      region.querySelectorAll("textarea, input[type=text], input:not([type])"),
      function (f) { return f.value.trim() !== ""; }
    );
  }
  document.addEventListener("htmx:beforeRequest", function (e) {
    var region = e.detail.elt;
    if (!region || !region.matches || !region.matches("[data-keep-drafts]")) return;
    if (hasDraft(region)) {
      region.dataset.stale = "1";
      e.preventDefault();
    }
  });
  function catchUp() {
    document.querySelectorAll("[data-keep-drafts][data-stale]").forEach(function (region) {
      if (hasDraft(region)) return;
      delete region.dataset.stale;
      htmx.trigger(region, "axl:refresh");
    });
  }
  ["focusout", "input", "toggle"].forEach(function (name) {
    document.addEventListener(name, function () { setTimeout(catchUp, 0); }, true);
  });

  // New task: the lead project cannot also be an extra project.
  function syncLead() {
    var lead = document.getElementById("new-project");
    if (!lead) return;
    document.querySelectorAll('input[name="also"]').forEach(function (box) {
      var isLead = box.value === lead.value;
      box.disabled = isLead;
      if (isLead) box.checked = false;
    });
  }
  document.addEventListener("change", function (e) {
    if (e.target && e.target.id === "new-project") syncLead();
  });
  syncLead();

  document.addEventListener("htmx:afterSettle", afterSwap);
  document.addEventListener("htmx:sseMessage", afterSwap);

  // Destructive forms ask first (`data-confirm` holds the question).
  document.addEventListener("submit", function (e) {
    var f = e.target;
    var question = f && f.getAttribute && f.getAttribute("data-confirm");
    if (question && !window.confirm(question)) e.preventDefault();
  });

  // The board's Key closes on a click outside it or on Escape.
  function closeKeys(except) {
    document.querySelectorAll("details.key[open]").forEach(function (d) {
      if (d !== except) d.open = false;
    });
  }
  document.addEventListener("click", function (e) {
    closeKeys(e.target instanceof Element ? e.target.closest("details.key") : null);
  });
  document.addEventListener("keydown", function (e) {
    if (e.key === "Escape") closeKeys(null);
  });

  document.addEventListener("change", function (e) {
    var t = e.target;
    if (t && t.hasAttribute && t.hasAttribute("data-autosubmit") && t.form) t.form.submit();
  });
})();
