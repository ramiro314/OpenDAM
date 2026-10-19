import { describe, expect, test } from 'bun:test';
import { COLLAPSE_SLACK, PANEL_LIMITS, clampPanelWidth, dragPanelWidth } from './panelSize';

const { sidebar, detail } = PANEL_LIMITS;

describe('clampPanelWidth', () => {
  test('keeps a width within the panel limits', () => {
    expect(clampPanelWidth('sidebar', 300)).toBe(300);
    expect(clampPanelWidth('sidebar', 50)).toBe(sidebar.min);
    expect(clampPanelWidth('detail', 5000)).toBe(detail.max);
  });

  test('leaves the grid its room, but never goes below the minimum', () => {
    expect(clampPanelWidth('detail', 500, 400)).toBe(400);
    expect(clampPanelWidth('detail', 500, 100)).toBe(detail.min);
  });

  test('rounds to whole pixels and replaces a bad stored width with the initial one', () => {
    expect(clampPanelWidth('sidebar', 250.6)).toBe(251);
    expect(clampPanelWidth('sidebar', NaN)).toBe(sidebar.initial);
  });
});

describe('dragPanelWidth', () => {
  test('dragging toward the grid widens both panels', () => {
    expect(dragPanelWidth('sidebar', 240, 40)).toBe(280);
    expect(dragPanelWidth('detail', 320, -40)).toBe(360);
  });

  test('dragging toward the window edge narrows them down to their minimum', () => {
    expect(dragPanelWidth('sidebar', 240, -40)).toBe(200);
    expect(dragPanelWidth('sidebar', 240, -(240 - sidebar.min + 10))).toBe(sidebar.min);
    expect(dragPanelWidth('detail', 320, 30)).toBe(290);
  });

  test('dragging well past the minimum collapses the panel', () => {
    const collapse = 240 - sidebar.min + COLLAPSE_SLACK + 1;
    expect(dragPanelWidth('sidebar', 240, -collapse)).toBeNull();
    expect(dragPanelWidth('sidebar', 240, -collapse + 2)).toBe(sidebar.min);
    expect(dragPanelWidth('detail', 320, 320 - detail.min + COLLAPSE_SLACK + 1)).toBeNull();
  });

  test('stops at the room the grid leaves', () => {
    expect(dragPanelWidth('sidebar', 240, 200, 300)).toBe(300);
  });
});
