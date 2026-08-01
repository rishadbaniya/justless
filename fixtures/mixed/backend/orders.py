from fastapi import APIRouter

router = APIRouter()


@router.get("/api/orders")
def list_orders():
    return database.execute("SELECT order_id FROM purchase_orders")
