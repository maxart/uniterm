"""Offline regression tests for source extraction and catalog generation."""
import copy
import json
import unittest

import generate


class CatalogTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.snapshot = json.loads((generate.HERE / 'sources.json').read_text())

    def test_every_available_source_has_unique_palette(self):
        rows = generate.catalog(self.snapshot)
        self.assertEqual(len(rows), 178)
        self.assertEqual(sum(r['origin'] == 'bundled' for _, _, r in rows), 22)
        self.assertEqual(sum(r['origin'] == 'gallery' for _, _, r in rows), 145)
        self.assertEqual(sum(r['origin'] == 'local' for _, _, r in rows), 11)
        self.assertEqual([r['name'] for r in self.snapshot['sources'] if 'error' in r], ['Gruvu'])
        for name, roles, _ in rows:
            self.assertEqual(len(roles), 8, name)
            self.assertNotEqual(roles[1], roles[2], name)
            self.assertNotEqual(roles[1], roles[4], name)

    def test_source_corruption_is_rejected(self):
        source = copy.deepcopy(self.snapshot['sources'][0])
        source['content'] += '\n'
        with self.assertRaisesRegex(ValueError, 'Checksum mismatch'):
            generate.palette(source)

    def test_alacritty_extraction_preserves_semantic_colors(self):
        source = next(r for r in self.snapshot['sources'] if r.get('path') == 'alacritty.toml')
        colors = generate.tomllib.loads(source['content'])['colors']
        roles = generate.palette(source)
        self.assertEqual(roles[0], generate.color(colors['primary']['background']))
        self.assertEqual(roles[2], generate.color(colors['primary']['foreground']))
        self.assertEqual(roles[5:], [generate.color(colors['normal'][key]) for key in ('green', 'yellow', 'red')])

    def test_minimal_matches_active_snapshot_and_attribution(self):
        source = next(r for r in self.snapshot['sources'] if r['name'] == 'minimal')
        self.assertEqual(source['sha256'], self.snapshot['local_observation']['active_palette_sha256'])
        self.assertIn('TyRichards', source['attribution'])
        self.assertEqual(generate.palette(source)[4], 0xd3d3d3)

    def test_duplicate_names_are_rejected(self):
        snapshot = copy.deepcopy(self.snapshot)
        snapshot['sources'].append(snapshot['sources'][0])
        with self.assertRaisesRegex(ValueError, 'Duplicate theme names'):
            generate.catalog(snapshot)


if __name__ == '__main__':
    unittest.main()
