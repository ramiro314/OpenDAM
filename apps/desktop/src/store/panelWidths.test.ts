import { afterEach, describe, expect, test } from 'bun:test';
import { persistedUI, useUIStore } from '.';
import { PANEL_LIMITS } from '../utils/panelSize';

const ui = () => useUIStore.getState();

afterEach(() => {
  useUIStore.setState({
    sidebarOpen: true,
    detailPanelOpen: true,
    sidebarWidth: PANEL_LIMITS.sidebar.initial,
    detailPanelWidth: PANEL_LIMITS.detail.initial,
  });
});

describe('setPanelWidth', () => {
  test("stores each side's width without touching the other", () => {
    ui().setPanelWidth('sidebar', 300);
    expect(ui().sidebarWidth).toBe(300);
    expect(ui().detailPanelWidth).toBe(PANEL_LIMITS.detail.initial);

    ui().setPanelWidth('detail', 400);
    expect(ui().detailPanelWidth).toBe(400);
    expect(ui().sidebarWidth).toBe(300);
  });

  test("clamps to the side's own limits", () => {
    ui().setPanelWidth('sidebar', 5000);
    expect(ui().sidebarWidth).toBe(PANEL_LIMITS.sidebar.max);
    ui().setPanelWidth('detail', 10);
    expect(ui().detailPanelWidth).toBe(PANEL_LIMITS.detail.min);
    expect(ui().sidebarWidth).toBe(PANEL_LIMITS.sidebar.max);
  });

  test('persists both widths', () => {
    ui().setPanelWidth('sidebar', 310);
    ui().setPanelWidth('detail', 410);
    const saved = persistedUI(ui());
    expect(saved.sidebarWidth).toBe(310);
    expect(saved.detailPanelWidth).toBe(410);
  });
});

describe('finishPanelDrag', () => {
  test('a drag that ends on a width stores it and leaves the panel open', () => {
    ui().finishPanelDrag('detail', 450);
    expect(ui().detailPanelOpen).toBe(true);
    expect(ui().detailPanelWidth).toBe(450);
  });

  test('a collapsing drag closes only its panel and keeps its width for reopening', () => {
    ui().setPanelWidth('sidebar', 360);
    ui().finishPanelDrag('sidebar', null);
    expect(ui().sidebarOpen).toBe(false);
    expect(ui().sidebarWidth).toBe(360);
    expect(ui().detailPanelOpen).toBe(true);

    ui().toggleSidebar();
    expect(ui().sidebarOpen).toBe(true);
    expect(ui().sidebarWidth).toBe(360);

    ui().finishPanelDrag('detail', null);
    expect(ui().detailPanelOpen).toBe(false);
    expect(ui().sidebarOpen).toBe(true);
  });
});
