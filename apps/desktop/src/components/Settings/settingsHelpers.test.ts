import { describe, expect, test } from 'bun:test';
import { isKnownModel, parseNumberInput, settingsPatch } from './settingsHelpers';
import type { AiSettings } from '../../types';

const saved: AiSettings = {
  anthropic: { model: 'claude-sonnet-5-5', concurrency: 4, maxTags: 8, maxProposals: 3 },
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
