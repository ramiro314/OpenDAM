import { describe, expect, test } from 'bun:test';
import {
  absoluteProblem,
  blenderLine,
  cacheLine,
  capProblem,
  claudePlanLines,
  exportSettingsPatch,
  ignoredOverridesNote,
  importBlenderLine,
  importSettingsPatch,
  importSettingsProblem,
  isKnownModel,
  parseNumberInput,
  rootCollectionOptions,
  settingsPatch,
  toolLine,
  unityPyLine,
} from './settingsHelpers';
import type { AiSettings, ClaudePlanStatus, Collection, ImportSettings } from '../../types';

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

describe('exportSettingsPatch', () => {
  const exportSaved = { root: '/home/me/export', rootCollection: 'dg', blenderPath: null };

  test('is null when nothing changed', () => {
    expect(exportSettingsPatch(exportSaved, { ...exportSaved })).toBeNull();
  });

  test('holds only the changed fields, with null for a cleared one', () => {
    expect(exportSettingsPatch(exportSaved, { ...exportSaved, root: null })).toEqual({
      root: null,
    });
  });
});

describe('blenderLine', () => {
  test('names the version and path of a Blender that answered', () => {
    const status = {
      series: '5.2',
      path: '/usr/bin/blender',
      version: '4.5.3',
      accepted: false,
      problem: 'x',
    };
    expect(blenderLine(status)).toBe('Blender 4.5.3 at /usr/bin/blender');
  });

  test('says why when no Blender answered', () => {
    const problem = 'No Blender at /opt/blender/blender';
    expect(
      blenderLine({ series: '5.2', path: null, version: null, accepted: false, problem })
    ).toBe(problem);
  });
});

describe('rootCollectionOptions', () => {
  const collection = (id: string, name: string, extra: Partial<Collection> = {}): Collection => ({
    id,
    name,
    description: '',
    assetIds: [],
    isSmart: false,
    createdAt: 0,
    ...extra,
  });

  test('offers only top-level, non-smart collections, by name', () => {
    const options = rootCollectionOptions([
      collection('dg', 'DungeonGame'),
      collection('dungeon', 'Dungeon', { parentId: 'dg' }),
      collection('recent', 'Recent', { isSmart: true, filterSpec: '{}' }),
      collection('art', 'Art'),
    ]);

    expect(options.map((c) => c.id)).toEqual(['art', 'dg']);
  });
});

const importSaved: ImportSettings = {
  blenderMemoryMax: null,
  unrealMemoryMax: '24G',
  unrealEditorRoot: null,
  assimpPath: null,
};

describe('importSettingsPatch', () => {
  test('is null when nothing changed, blanks included', () => {
    expect(importSettingsPatch(importSaved, { ...importSaved })).toBeNull();
    expect(importSettingsPatch(importSaved, { ...importSaved, assimpPath: '  ' })).toBeNull();
  });

  test('holds the changed fields trimmed, and clears an emptied one', () => {
    const draft = {
      ...importSaved,
      blenderMemoryMax: ' 16G ',
      unrealMemoryMax: '',
      unrealEditorRoot: '/opt/UnrealEngine',
    };
    expect(importSettingsPatch(importSaved, draft)).toEqual({
      blenderMemoryMax: '16G',
      unrealMemoryMax: null,
      unrealEditorRoot: '/opt/UnrealEngine',
    });
  });
});

describe('capProblem', () => {
  test('accepts what systemd reads, and an empty field for the default', () => {
    for (const ok of [null, '', ' ', '12G', '512M', '1T', '2048', '010K']) {
      expect(capProblem(ok)).toBeNull();
    }
  });

  test('refuses a zero cap, a unit alone, decimals and other words', () => {
    for (const bad of ['0G', '000', 'G', '1.5G', '12GB', '12g', 'lots', '-1G']) {
      expect(capProblem(bad)).toBe('Give a size such as 12G (K, M, G or T)');
    }
  });
});

describe('absoluteProblem', () => {
  test('accepts an absolute path or nothing', () => {
    expect(absoluteProblem(null)).toBeNull();
    expect(absoluteProblem('')).toBeNull();
    expect(absoluteProblem(' /usr/bin/assimp ')).toBeNull();
    expect(absoluteProblem('bin/assimp')).toBe('Give an absolute path, starting with /');
    expect(absoluteProblem('~/Applications/UnrealEngine')).not.toBeNull();
  });
});

describe('importSettingsProblem', () => {
  test('names the first field that cannot be saved', () => {
    expect(importSettingsProblem(importSaved)).toBeNull();
    expect(
      importSettingsProblem({ ...importSaved, unrealMemoryMax: '0G', assimpPath: 'assimp' })
    ).toBe("The Unreal Editor's memory cap: Give a size such as 12G (K, M, G or T)");
    expect(importSettingsProblem({ ...importSaved, assimpPath: 'assimp' })).toBe(
      "assimp's path: Give an absolute path, starting with /"
    );
  });
});

describe('toolLine', () => {
  test('names the version and path, else why there is none', () => {
    expect(toolLine('assimp', { path: '/usr/bin/assimp', version: '6.0', problem: null })).toBe(
      'assimp 6.0 at /usr/bin/assimp'
    );
    const why = 'Exporting from Unreal needs the Unreal Editor: no editor at /x';
    expect(toolLine('Unreal Editor', { path: null, version: null, problem: why })).toBe(why);
    expect(toolLine('assimp', { path: null, version: null, problem: null })).toBe(
      'assimp not found'
    );
  });
});

describe('unityPyLine', () => {
  test('says whether UnityPy imports, or that the engine has not said yet', () => {
    expect(unityPyLine(true)).toBe('UnityPy imports in the preview engine');
    expect(unityPyLine(false)).toContain("UnityPy can't be imported");
    expect(unityPyLine(null)).toContain('Not known yet');
  });
});

describe('cacheLine', () => {
  test('gives the size and where the cache is', () => {
    const path = '/home/me/.cache/opendam/import';
    expect(cacheLine({ path, bytes: 0 })).toBe(`Conversion cache: Empty, at ${path}`);
    expect(cacheLine({ path, bytes: 3 * 1024 * 1024 })).toBe(`Conversion cache: 3.0MB, at ${path}`);
  });
});

describe('importBlenderLine', () => {
  const found = {
    series: '5.2',
    path: '/usr/bin/blender',
    version: '5.2.2',
    accepted: true,
    problem: null,
  };

  test("shows the check's answer, else its error", () => {
    expect(importBlenderLine(found, null, null)).toBe('Blender 5.2.2 at /usr/bin/blender');
    expect(importBlenderLine(null, 'IPC failed', null)).toBe('IPC failed');
  });

  test('waits for the check, unless Blender’s path never loaded, which it says', () => {
    expect(importBlenderLine(null, null, null)).toBe('Checking Blender…');
    expect(importBlenderLine(null, null, 'database is locked')).toBe(
      "Not checked: Blender's path couldn't be loaded (database is locked)"
    );
  });
});
