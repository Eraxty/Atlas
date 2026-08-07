from src.article import Article

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
            subject= header["subject"],
            author= header["from"],
            date= header["date"],
            message_id= header["message-id"],
            references= header["references"],
            bytes= to_int(header[":bytes" if ":bytes" in header else "bytes"]),
            lines= to_int(header[":lines" if ":lines" in header else "lines"])
        )

        articles.append(article)

    return articles
