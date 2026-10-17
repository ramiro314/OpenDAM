import { describe, expect, test } from 'bun:test';
import {
  claudePlanLines,
  ignoredOverridesNote,
  isKnownModel,
  parseNumberInput,
  settingsPatch,
} from './settingsHelpers';
import type { AiSettings, ClaudePlanStatus } from '../../types';

const saved: AiSettings = {
  anthropic: {
    access: 'api_key',
    model: 'claude-sonnet-5-5',
    concurrency: 4,
    maxTags: 8,
    maxProposals: 3,
  },
  jev: { concurrency: 4, threshold: 0.7, maxTags: 8 },
};

describe('settingsPatch', () => {
  test('is null when nothing changed', () => {
    expect(settingsPatch(saved, structuredClone(saved))).toBeNull();
  });

  test('holds only the changed fields', () => {
    const draft = structuredClone(saved);
    draft.anthropic.model = 'my-model';
    draft.jev.threshold = 0.9;

    expect(settingsPatch(saved, draft)).toEqual({
      anthropic: { model: 'my-model' },
      jev: { threshold: 0.9 },
    });
  });
});

describe('isKnownModel', () => {
  test('knows the Claude 5.5 models and treats any other id as custom', () => {
    expect(isKnownModel('claude-haiku-5-5')).toBe(true);
    expect(isKnownModel('claude-sonnet-4-6')).toBe(false);
  });
});

describe('parseNumberInput', () => {
  test('an emptied field commits nothing rather than 0', () => {
    expect(parseNumberInput('', true)).toBeNull();
    expect(parseNumberInput('  ', false)).toBeNull();
  });

  test('integer fields take only whole numbers', () => {
    expect(parseNumberInput('12', true)).toBe(12);
    expect(parseNumberInput('4.5', true)).toBeNull();
  });

  test('fractional fields take decimals, and nothing that is not a number', () => {
    expect(parseNumberInput('0.85', false)).toBe(0.85);
    expect(parseNumberInput('-', false)).toBeNull();
  });
});

const loggedIn: ClaudePlanStatus = {
  path: '/usr/bin/claude',
  version: '2.1.296 (Claude Code)',
  loggedIn: true,
  authMethod: 'claude.ai',
  subscriptionType: 'max',
  configured: true,
  problem: null,
  ignoredOverrides: ['ANTHROPIC_API_KEY'],
};

describe('claudePlanLines', () => {
  test('names the path, the version and the claude.ai login', () => {
    expect(claudePlanLines(loggedIn)).toEqual({
      found: 'claude 2.1.296 (Claude Code) at /usr/bin/claude',
      login: 'Logged in with your Claude account (claude.ai, max plan)',
    });
  });

  test('says when the login is not your Claude plan, or there is none', () => {
    const apiKey = { ...loggedIn, authMethod: 'api_key', configured: false };
    expect(claudePlanLines(apiKey).login).toBe('Logged in with api_key, not your Claude plan');
    const out = { ...loggedIn, loggedIn: false, authMethod: 'none', configured: false };
    expect(claudePlanLines(out).login).toBe('Logged out');
    const missing = { ...out, path: null, version: null };
    expect(claudePlanLines(missing)).toEqual({
      found: 'claude not found on PATH',
      login: 'Not checked',
    });
  });
});

describe('ignoredOverridesNote', () => {
  test('names the overrides claude never gets', () => {
    expect(ignoredOverridesNote([])).toBeNull();
    expect(ignoredOverridesNote(['ANTHROPIC_API_KEY'])).toBe(
      'ANTHROPIC_API_KEY is set for the app, but claude never gets it'
    );
    expect(ignoredOverridesNote(['ANTHROPIC_API_KEY', 'ANTHROPIC_BASE_URL'])).toBe(
      'ANTHROPIC_API_KEY, ANTHROPIC_BASE_URL are set for the app, but claude never gets them'
    );
  });
});
