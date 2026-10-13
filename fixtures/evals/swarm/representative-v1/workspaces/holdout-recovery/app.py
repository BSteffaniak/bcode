from decode import decode
from publish import publish, TransientError
def evaluate(request):
    """Decode then retry transient publication up to three attempts. Return published, attempts, commits. Exhaustion raises TransientError. Do not mutate request."""
    state = {"attempts":0,"commits":[]}
    values = decode(request["rows"])
    published = publish(values,state,request["failures"])
    return {"published":published,**state}
