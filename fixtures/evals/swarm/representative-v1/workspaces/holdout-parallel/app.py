import intervals
import routes
import metrics
import tags
def evaluate(request):
    """Integrate every independent module, preserving request."""
    return {'intervals': intervals.solve(request['intervals']), 'routes': routes.solve(request['routes']), 'metrics': metrics.solve(request['metrics']), 'tags': tags.solve(request['tags'])}
