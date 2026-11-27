<script lang="ts">
  // The persistent title bar of the undecorated window (Windows; see
  // lib/titlebar.ts): a solid strip across the top `--titlebar-h` band with the
  // app's mark and name. It sits over every shell, under the caption buttons
  // (WindowControls), and has no controls of its own, so a press anywhere on it
  // drags the window and a double-click maximizes (App.svelte's
  // `onTitlebarPress`). Shells start below it; nothing scrolls under it.
  import { t } from "../i18n.svelte";
</script>

<div class="titlebar" aria-hidden="true">
  <img class="mark" src="/favicon.svg" alt="" width="16" height="16" draggable="false" />
  <span class="name">{t("window.title")}</span>
</div>

<style>
  .titlebar {
    position: fixed;
    top: 0;
    left: 0;
    right: 0;
    /* Over the shells, the boot screen and modal veils; under the drag ghost
       and the caption buttons (both 1000). */
    z-index: 999;
    box-sizing: border-box;
    height: var(--titlebar-h);
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 0 12px;
    background: var(--surface-chrome);
    backdrop-filter: blur(var(--glass-blur));
    border-bottom: 1px solid var(--line);
    color: var(--text-muted);
    font: 12px/1 var(--font-ui);
    user-select: none;
  }
  .mark {
    flex: none;
    pointer-events: none;
  }
  .name {
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
  }
</style>
