from pagination import collect

pages = {None: {"items": [1], "next": "a"}, "a": {"items": [2], "next": None}}
assert collect(pages.__getitem__) == [1, 2]
pages = {None: {"items": [], "next": "a"}, "a": {"items": [3], "next": None}}
assert collect(pages.__getitem__) == [3]
print("public checks passed")
