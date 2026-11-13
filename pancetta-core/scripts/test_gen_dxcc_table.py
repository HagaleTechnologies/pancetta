import importlib.util
import tempfile
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location('gen', Path(__file__).with_name('gen_dxcc_table.py'))
gen = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gen)


class GenerationTests(unittest.TestCase):
    def fixture(self, text):
        tmp = tempfile.NamedTemporaryFile(mode='w', delete=False)
        tmp.write(text)
        tmp.close()
        self.addCleanup(Path(tmp.name).unlink)
        return tmp.name

    def test_modifiers_slash_primaries_and_exact_only(self):
        path = self.fixture('Bouvet: 1: 1: AF: 0: 0: 0: 3Y/b:\n =3Y0J(1)[2]<3/4>{AF}~0~,=3Y0J;')
        rows = list(gen.parse_records(path))
        self.assertEqual(rows, [('Bouvet', '3Y/B', [], ['3Y0J', '3Y0J'])])

    def test_missing_and_conflicting_mappings_fail(self):
        rows = [('Japan', 'JA', [], [])]
        with self.assertRaises(ValueError):
            gen.supplemental_tables(rows, {}, {}, [])
        with self.assertRaises(ValueError):
            gen.supplemental_tables(rows, {'JA': {'name': 'Japan', 'id': 291}}, {'339': 'JAPAN'}, [])

    def test_alias_parent_determinism_pruning_and_conflicts(self):
        mapping = {'K': {'name': 'United States', 'id': 291}, 'KG4': {'name': 'Guantanamo Bay', 'id': 105},
                   'TA': {'name': 'Asiatic Turkey', 'id': 390}, 'TA1': {'name': 'European Turkey', 'id': 390, 'canonical': 'Asiatic Turkey'}}
        ids = {'291': 'USA', '105': 'GUANTANAMO', '390': 'TURKEY'}
        rows = [('United States', 'K', [], ['K1ABC', 'KG4AB']), ('Guantanamo Bay', 'KG4', [], ['KG4AC']),
                ('Asiatic Turkey', 'TA', [], []), ('European Turkey', '*TA1', [], [])]
        base = [('KG4', 'Guantanamo Bay'), ('K', 'United States')]
        first = gen.supplemental_tables(rows, mapping, ids, base)
        self.assertEqual(first, gen.supplemental_tables(list(reversed(rows)), mapping, ids, base))
        self.assertEqual(first[1], [('KG4AB', 'United States')])
        rows.append(('Guantanamo Bay', 'KG4', [], ['KG4AB']))
        with self.assertRaises(ValueError):
            gen.supplemental_tables(rows, mapping, ids, base)


    def test_aliases_and_conflicting_name_identity(self):
        rows = [('Cabo Verde', 'D4', [], ['D4ABC'])]
        mapping = {'D4': {'name': 'Cape Verde', 'aliases': ['Cabo Verde'], 'id': 409}}
        registry, exact = gen.supplemental_tables(rows, mapping, {'409': 'CAPE VERDE'}, [('D4', 'Cape Verde')])
        self.assertEqual(registry, [('D4', 'Cape Verde', 409, 'Cape Verde')])
        self.assertEqual(exact, [])
        rows.append(('Cape Verde', 'D5', [], []))
        mapping['D5'] = {'name': 'Cape Verde', 'id': 291}
        with self.assertRaises(ValueError):
            gen.supplemental_tables(rows, mapping, {'409': 'CAPE VERDE', '291': 'USA'}, [])

    def test_zone_defaults_and_independent_modifiers(self):
        path = self.fixture('Old Name: 5: 8: NA: 0: 0: 0: *K:\n K(4),K0(4)[7],W[6],N<1/2>{NA}~3~,=N2NL/MM(7),=N2NL/MM(7);')
        mapping = {'K': {'name': 'United States', 'aliases': ['Old Name']}}
        defaults, prefixes, exact = gen.zone_tables(path, mapping)
        self.assertEqual(defaults, [('United States', 5, 8)])
        self.assertEqual(prefixes, [('K', 'United States', 4, 8), ('K0', 'United States', 4, 7),
                                    ('N', 'United States', 5, 8), ('W', 'United States', 5, 6)])
        self.assertEqual(exact, [('N2NL/MM', 'United States', 7, 8)])

    def test_zone_exact_rows_survive_entity_pruning(self):
        path = self.fixture('United States: 5: 8: NA: 0: 0: 0: K:\n =K1ABC(4)[7];\nBouvet: 38: 67: AF: 0: 0: 0: 3Y/b:\n =3Y0J;')
        mapping = {'K': {'name': 'United States', 'id': 291}, '3Y/B': {'name': 'Bouvet', 'id': 24}}
        _, entity_exact = gen.supplemental_tables(list(gen.parse_records(path)), mapping,
                                                {'291': 'USA', '24': 'BOUVET'}, [('K', 'United States')])
        self.assertNotIn(('K1ABC', 'United States'), entity_exact)
        defaults, prefixes, exact = gen.zone_tables(path, mapping)
        self.assertIn(('K1ABC', 'United States', 4, 7), exact)
        self.assertIn(('3Y0J', 'Bouvet', 38, 67), exact)
        self.assertEqual(prefixes, [('K', 'United States', 5, 8)])

    def test_zone_collisions_and_invalid_values(self):
        mapping = {'K': {'name': 'United States'}, 'X': {'name': 'Other'}}
        path = self.fixture('United States: 5: 8: NA: 0: 0: 0: K:\n X(4),Y(3),=CALL(2);\nOther: 10: 20: NA: 0: 0: 0: X:\n Y(11),=CALL(12);')
        _, prefixes, exact = gen.zone_tables(path, mapping)
        self.assertEqual(prefixes, [('K', 'United States', 5, 8), ('X', 'Other', 10, 20), ('Y', 'United States', 3, 8)])
        self.assertEqual(exact, [('CALL', 'Other', 12, 20), ('CALL', 'United States', 2, 8)])
        for token in ['K0(0)', 'K0(41)', 'K0[91]', 'K0[0]', 'K0(no)', 'K0(4', 'K0[7', 'K0(4)(5)', 'K0(4),K0(5)', '=K1ABC(4),=K1ABC(5)']:
            with self.subTest(token=token), self.assertRaises(ValueError):
                gen.zone_tables(self.fixture('United States: 5: 8: NA: 0: 0: 0: K:\n ' + token + ';'), mapping)
        for cq, itu in [(0, 8), (41, 8), (5, 91), (5, 0), ('bad', 8)]:
            with self.subTest(cq=cq, itu=itu), self.assertRaises(ValueError):
                gen.zone_tables(self.fixture(f'United States: {cq}: {itu}: NA: 0: 0: 0: K:\n K;'), mapping)

    def test_checked_in_outputs_are_reproducible(self):
        core = Path(__file__).resolve().parents[1]
        expected = gen.generate_supplemental(core)
        self.assertEqual(set(expected), {'dxcc_registry.rs', 'dxcc_exact.rs', 'dxcc_zones.rs'})
        defaults, prefixes, exact = gen.zone_tables(core / 'data/cty.dat', __import__('json').loads((core / 'data/dxcc_adif_map.json').read_text())['entities'])
        for row in [*defaults, *prefixes, *exact]:
            self.assertTrue(1 <= row[-2] <= 40)
            self.assertTrue(1 <= row[-1] <= 90)
        for name, text in expected.items():
            self.assertEqual((core / 'src' / name).read_text(), text)


if __name__ == '__main__':
    unittest.main()
