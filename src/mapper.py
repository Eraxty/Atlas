from email.utils import parsedate_to_datetime

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


def to_iso_date(value):
    try:
        return parsedate_to_datetime(value).strftime("%Y-%m-%d %H:%M:%S")
    
    except (TypeError, ValueError, OverflowError):
        return value


def headers_to_articles(headers):
    articles = []

    for number, header in headers:
        article = Article(
            number= number,
            subject= clean(header.get("subject", "")),
            author= clean(header.get("from", "")),
            date= to_iso_date(clean(header.get("date", ""))),
            message_id= clean(header.get("message-id", "")),
            references= clean(header.get("references", "")),
            bytes= to_int(header.get(":bytes" if ":bytes" in header else "bytes", 0)),
            lines= to_int(header.get(":lines" if ":lines" in header else "lines", 0))
        )

        articles.append(article)

    return articles
