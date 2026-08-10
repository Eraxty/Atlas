from src.article import Article

def clean(value):
    if isinstance(value, str):
        return value.encode("utf-8", "replace").decode("utf-8")
    return value

def to_int(value):
    try:
        return int(value)
    except (TypeError, ValueError):
        return 0


def headers_to_articles(headers):
    articles = []

    for number, header in headers:
        article = Article(
            number= number,
            subject= clean(header["subject"]),
            author= clean(header["from"]),
            date= clean(header["date"]),
            message_id= clean(header["message-id"]),
            references= clean(header["references"]),
            bytes= to_int(header[":bytes" if ":bytes" in header else "bytes"]),
            lines= to_int(header[":lines" if ":lines" in header else "lines"])
        )

        articles.append(article)

    return articles
