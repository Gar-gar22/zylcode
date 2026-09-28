# statslib — First Mission fixture

A minimal Python statistics library used as the target project for the ZylCode
First Mission (Master Transformation Prompt §36, phase T3 of
`docs/governance/ZYLCODE_TRANSFORMATION_PLAN.md`).

This fixture contains one **deliberate defect**: `mean()` divides by
`len(values) - 1`. The baseline suite does not cover `mean()`, so it passes.
A mission specification adds coverage for `mean()`, observes the real failure,
diagnoses it from the real traceback, repairs the implementation, and
re-verifies with the same real test command.

Run the baseline suite:

```sh
python -m unittest discover -v -s . -p "test_*.py"
```
