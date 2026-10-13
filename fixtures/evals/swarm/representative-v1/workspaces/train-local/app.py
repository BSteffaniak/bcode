from parse import amount
from validate import accepted
def evaluate(rows):
    """Sum posted amounts; empty input is zero. Never mutate rows."""
    return sum(amount(row["amount"]) for row in rows if accepted(row))
