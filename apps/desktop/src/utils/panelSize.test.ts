import { describe, expect, test } from 'bun:test';
import {
  COLLAPSE_SLACK,
  KEY_STEP,
  PANEL_LIMITS,
  clampPanelWidth,
  dragPanelWidth,
  keyPanelWidth,
  settledPanelWidth,
} from './panelSize';

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

  test('a drag landing exactly on the collapse threshold keeps the panel at its minimum', () => {
    expect(dragPanelWidth('sidebar', 240, -(240 - sidebar.min + COLLAPSE_SLACK))).toBe(sidebar.min);
    expect(dragPanelWidth('detail', 320, 320 - detail.min + COLLAPSE_SLACK)).toBe(detail.min);
  });

  test('stops at the room the grid leaves', () => {
    expect(dragPanelWidth('sidebar', 240, 200, 300)).toBe(300);
  });
});

describe('keyPanelWidth', () => {
  test('the arrow toward the grid widens each panel', () => {
    expect(keyPanelWidth('sidebar', 240, 'ArrowRight')).toBe(240 + KEY_STEP);
    expect(keyPanelWidth('sidebar', 240, 'ArrowLeft')).toBe(240 - KEY_STEP);
    expect(keyPanelWidth('detail', 320, 'ArrowLeft')).toBe(320 + KEY_STEP);
    expect(keyPanelWidth('detail', 320, 'ArrowRight')).toBe(320 - KEY_STEP);
  });

  test('stops at the limits and the room, never collapsing', () => {
    expect(keyPanelWidth('sidebar', sidebar.min, 'ArrowLeft')).toBe(sidebar.min);
    expect(keyPanelWidth('detail', detail.max, 'ArrowLeft')).toBe(detail.max);
    expect(keyPanelWidth('detail', 400, 'ArrowLeft', 405)).toBe(405);
  });

  test('ignores other keys', () => {
    expect(keyPanelWidth('sidebar', 240, 'ArrowUp')).toBeNull();
    expect(keyPanelWidth('sidebar', 240, 'Enter')).toBeNull();
  });
});

describe('settledPanelWidth', () => {
  test('a panel showing its stored width keeps it', () => {
    expect(settledPanelWidth('sidebar', 360, 360)).toBeNull();
    expect(settledPanelWidth('sidebar', 359.7, 360)).toBeNull();
  });

  test('a panel a narrow window shrank stores what it shows, rounded down', () => {
    expect(settledPanelWidth('sidebar', 295.4, 360)).toBe(295);
    expect(settledPanelWidth('detail', 345.9, 420)).toBe(345);
  });

  test('never below the minimum, and a stored width out of limits counts as clamped', () => {
    expect(settledPanelWidth('sidebar', sidebar.min - 0.5, 300)).toBe(sidebar.min);
    expect(settledPanelWidth('detail', detail.max, 5000)).toBeNull();
  });
});
