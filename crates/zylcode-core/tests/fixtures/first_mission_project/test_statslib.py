import unittest

from statslib import median


class TestMedian(unittest.TestCase):
    def test_odd_count(self):
        self.assertEqual(median([1, 3, 2]), 2.0)

    def test_even_count(self):
        self.assertEqual(median([1, 2, 3, 4]), 2.5)

    def test_empty_raises(self):
        with self.assertRaises(ValueError):
            median([])


if __name__ == "__main__":
    unittest.main()
