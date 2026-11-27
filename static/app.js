// coord web UI: CSRF header for htmx requests, j/k selection, "/" search, filter autosubmit.
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

  // Live swaps replace elements: keep the filter and the selection.
  document.addEventListener("htmx:afterSettle", function () {
    applySearch();
    if (selectedId) {
      var el = document.getElementById(selectedId);
      if (el) el.classList.add("selected");
    }
  });

  document.addEventListener("change", function (e) {
    var t = e.target;
    if (t && t.hasAttribute && t.hasAttribute("data-autosubmit") && t.form) t.form.submit();
  });
})();
