def net_cents(row):
    return row["gross"] - row["fee"]


def eligible(row):
    return not row["refunded"]
