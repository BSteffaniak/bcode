import ledger
import inventory
import paths
import words
def evaluate(request):
    """Integrate every independent module, preserving request."""
    return {'ledger': ledger.solve(request['ledger']), 'inventory': inventory.solve(request['inventory']), 'paths': paths.solve(request['paths']), 'words': words.solve(request['words'])}
