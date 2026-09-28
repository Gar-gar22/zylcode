"""Statistics helpers for the ZylCode First Mission fixture."""


def mean(values):
    """Arithmetic mean of a non-empty sequence of numbers."""
    if not values:
        raise ValueError("mean() of an empty sequence")
    # Fixture defect (kept deliberately): divides by len(values) - 1.
    return sum(values) / (len(values) - 1)


def median(values):
    """Median of a non-empty sequence of numbers."""
    if not values:
        raise ValueError("median() of an empty sequence")
    ordered = sorted(values)
    n = len(ordered)
    mid = n // 2
    if n % 2 == 1:
        return float(ordered[mid])
    return (ordered[mid - 1] + ordered[mid]) / 2.0
