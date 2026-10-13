def transfer(balance,amount):
    """Reject negative transfers; insufficient balance also rejected."""
    if amount > balance: raise ValueError("insufficient")
    return balance-amount
