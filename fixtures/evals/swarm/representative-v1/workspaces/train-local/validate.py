def accepted(row):
    """Only status=posted rows contribute; absent status is pending."""
    return row.get("status", "posted") == "posted"
