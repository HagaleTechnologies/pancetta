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

    def test_checked_in_outputs_are_reproducible(self):
        core = Path(__file__).resolve().parents[1]
        expected = gen.generate_supplemental(core)
        for name, text in expected.items():
            self.assertEqual((core / 'src' / name).read_text(), text)


if __name__ == '__main__':
    unittest.main()
