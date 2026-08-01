from collections import defaultdict

def get_categories(client):
    categories = defaultdict(list)

    for group in client.list_groups():
        
        group = group.strip()
        print(group)
        name = group.split()[0]
        parts = name.split(".")

        if len(parts) < 3:
            continue

        category = parts[2]
        categories[category].append(name)

    return categories