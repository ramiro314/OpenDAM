import { describe, expect, test } from 'bun:test';
import { folderInLibraries, normalizeFolderPath } from './folders';

describe('normalizeFolderPath', () => {
  test('drops padding and edge slashes', () => {
    expect(normalizeFolderPath(' /3D assets/Platformer Kit/ ')).toBe('3D assets/Platformer Kit');
  });

  test('turns only slashes into the root', () => {
    expect(normalizeFolderPath('')).toBe('');
    expect(normalizeFolderPath(' / ')).toBe('');
  });
});

describe('folderInLibraries', () => {
  const libraries = [
    { id: 'kenney', rootPaths: ['/home/me/Kenney/Assets'] },
    { id: 'nested', rootPaths: ['/home/me/Kenney/Assets/3D assets/'] },
    { id: 'other', rootPaths: ['/home/me/Other'] },
  ];

  test('strips the library root from a picked folder', () => {
    expect(folderInLibraries('/home/me/Kenney/Assets/2D assets/Tiles', libraries)).toEqual({
      libraryId: 'kenney',
      path: '2D assets/Tiles',
    });
  });

  test('picks the innermost library when roots nest', () => {
    expect(folderInLibraries('/home/me/Kenney/Assets/3D assets/Platformer Kit', libraries)).toEqual(
      { libraryId: 'nested', path: 'Platformer Kit' }
    );
  });

  test('maps a library root to the empty path', () => {
    expect(folderInLibraries('/home/me/Other/', libraries)).toEqual({
      libraryId: 'other',
      path: '',
    });
  });

  test('is null outside every library, even beside a root sharing its name', () => {
    expect(folderInLibraries('/home/me/Other Stuff/a', libraries)).toBeNull();
    expect(folderInLibraries('/elsewhere', libraries)).toBeNull();
  });
});
