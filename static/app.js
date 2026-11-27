// coord web UI: CSRF header for htmx requests, j/k selection, "/" search, filter autosubmit,
// confirmations on destructive forms.
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

  // Live swaps replace elements: keep the filter, the selection and open subtask lists.
  function afterSwap() {
    applySearch();
    restoreOpen();
    if (selectedId) {
      var el = document.getElementById(selectedId);
      if (el) el.classList.add("selected");
    }
  }
  document.addEventListener("htmx:afterSettle", afterSwap);
  document.addEventListener("htmx:sseMessage", afterSwap);

  // Destructive forms ask first (`data-confirm` holds the question).
  document.addEventListener("submit", function (e) {
    var f = e.target;
    var question = f && f.getAttribute && f.getAttribute("data-confirm");
    if (question && !window.confirm(question)) e.preventDefault();
  });

  document.addEventListener("change", function (e) {
    var t = e.target;
    if (t && t.hasAttribute && t.hasAttribute("data-autosubmit") && t.form) t.form.submit();
  });
})();
